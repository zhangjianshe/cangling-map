//! Stateless Streamable HTTP (2025-06-18), exposing only public read-only catalog tools.
use crate::{tiles3d, AppState};
use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

const VERSIONS: &[&str] = &["2025-03-26", "2025-06-18", "2025-11-25"];
fn origin_allowed(headers: &HeaderMap) -> bool {
    match headers.get("origin") {
        None => true, // Native MCP clients do not send a browser origin.
        Some(origin) => origin.to_str().ok().is_some_and(|origin| {
            std::env::var("MAP_MCP_ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .any(|s| !s.trim().is_empty() && s.trim() == origin)
        }),
    }
}
pub(crate) async fn get(headers: HeaderMap) -> Response {
    if !origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    (StatusCode::METHOD_NOT_ALLOWED, [("Allow", "POST")]).into_response()
}
fn rpc_error(id: Value, code: i64, message: &str) -> Response {
    Json(json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})).into_response()
}
pub(crate) fn tools() -> Value {
    let search = json!({"type":"object","properties":{"query":{"type":"string","maxLength":128},"bounds":{"type":"array","minItems":4,"maxItems":4,"items":{"type":"number"}},"limit":{"type":"integer","minimum":1,"maximum":100}},"additionalProperties":false});
    let detail = json!({"type":"object","properties":{"datasetId":{"type":"string","minLength":1,"maxLength":64}},"required":["datasetId"],"additionalProperties":false});
    json!([
        {"name":"search_3d_datasets","description":"查询已发布且启用的三维建筑/3D Tiles数据集。query匹配名称或ID；bounds为WGS84西南东北包围范围，不跨日期变更线。返回覆盖范围、版本和tileset入口，未知范围为null。结果不是逐栋建筑属性或渲染确认。","inputSchema":search,"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}},
        {"name":"get_3d_dataset","description":"根据真实datasetId读取当前发布版本及tileset入口。用返回tilesetUrl调用客户端earth_add_tileset，再根据bounds定位并检查earth_get_view；Map不操作客户端窗口。不存在或停用时返回错误，不编造数据。","inputSchema":detail,"annotations":{"readOnlyHint":true,"destructiveHint":false,"idempotentHint":true,"openWorldHint":false}}
    ])
}
fn query(name: &str, args: &Value) -> Result<tiles3d::DiscoveryQuery, String> {
    let object = args.as_object().ok_or("arguments必须为对象")?;
    let mut q = json!({});
    match name {
        "search_3d_datasets" => {
            if object
                .keys()
                .any(|s| !matches!(s.as_str(), "query" | "bounds" | "limit"))
            {
                return Err("未知查询参数".into());
            }
            if let Some(v) = object.get("query") {
                let text = v
                    .as_str()
                    .filter(|s| s.chars().count() <= 128)
                    .ok_or("query须为不超过128字的文本")?;
                q["q"] = json!(text);
            }
            if let Some(v) = object.get("bounds") {
                let a = v
                    .as_array()
                    .filter(|a| a.len() == 4)
                    .ok_or("bounds须含西南东北四个数值")?;
                if !a.iter().all(|v| v.as_f64().is_some_and(f64::is_finite)) {
                    return Err("bounds须为有限数值".into());
                }
                q["bbox"] = json!(a.iter().map(Value::to_string).collect::<Vec<_>>().join(","));
            }
            if let Some(v) = object.get("limit") {
                let n = v
                    .as_u64()
                    .filter(|n| (1..=100).contains(n))
                    .ok_or("limit须为1–100整数")?;
                q["limit"] = json!(n);
            }
        }
        "get_3d_dataset" => {
            if object.keys().any(|s| s != "datasetId") {
                return Err("未知数据集参数".into());
            }
            let id = args["datasetId"].as_str().ok_or("缺少datasetId")?;
            map_3dtiles::valid_id(id)?;
            q["id"] = json!(id);
        }
        _ => return Err("未知工具".into()),
    }
    serde_json::from_value(q).map_err(|e| e.to_string())
}
fn public_urls(data: &mut Value) -> Result<(), String> {
    // Keep tool output bounded and avoid forwarding arbitrary publisher extras to the model.
    for d in data["datasets"].as_array_mut().into_iter().flatten() {
        d["source"] = d["metadata"]["source"]
            .as_str()
            .map(|s| json!(s.chars().take(256).collect::<String>()))
            .unwrap_or(Value::Null);
        d["description"] = d["metadata"]["description"]
            .as_str()
            .map(|s| json!(s.chars().take(1000).collect::<String>()))
            .unwrap_or(Value::Null);
        if let Some(object) = d.as_object_mut() {
            object.remove("metadata");
        }
    }

    // A configured canonical URL preserves reverse-proxy prefixes without trusting Host/Forwarded.
    let Ok(raw) = std::env::var("MAP_PUBLIC_BASE_URL") else {
        return Ok(());
    };
    let base = reqwest::Url::parse(&format!("{}/", raw.trim_end_matches('/')))
        .map_err(|_| "MAP_PUBLIC_BASE_URL无效")?;
    if !matches!(base.scheme(), "http" | "https")
        || !base.username().is_empty()
        || base.password().is_some()
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err("MAP_PUBLIC_BASE_URL须为无凭证HTTP(S)服务根地址".into());
    }
    for d in data["datasets"].as_array_mut().into_iter().flatten() {
        if let Some(path) = d["tilesetPath"].as_str() {
            d["tilesetUrl"] = json!(base.join(path).map_err(|e| e.to_string())?.as_str());
        }
    }
    Ok(())
}
pub(crate) async fn post(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !origin_allowed(&headers) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(';').next() == Some("application/json"))
    {
        return StatusCode::UNSUPPORTED_MEDIA_TYPE.into_response();
    }
    let accept = headers
        .get("accept")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        return StatusCode::NOT_ACCEPTABLE.into_response();
    }
    if headers
        .get("mcp-protocol-version")
        .is_some_and(|v| !v.to_str().ok().is_some_and(|v| VERSIONS.contains(&v)))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return rpc_error(Value::Null, -32700, "Parse error"),
    };
    let id = v.get("id").cloned().unwrap_or(Value::Null);
    if !v.is_object()
        || v["jsonrpc"] != "2.0"
        || !v["method"].is_string()
        || v.get("id")
            .is_some_and(|id| !id.is_string() && !id.is_i64() && !id.is_u64())
    {
        return rpc_error(Value::Null, -32600, "Invalid Request");
    }
    let method = v["method"].as_str().unwrap();
    if v.get("id").is_none() {
        return if matches!(
            method,
            "notifications/initialized" | "notifications/cancelled"
        ) {
            StatusCode::ACCEPTED.into_response()
        } else {
            StatusCode::BAD_REQUEST.into_response()
        };
    }
    let params = v.get("params").cloned().unwrap_or(json!({}));
    if !params.is_object() {
        return rpc_error(id, -32602, "Invalid params");
    }
    let result = match method {
        "initialize" => {
            let Some(requested) = params["protocolVersion"].as_str() else {
                return rpc_error(id, -32602, "protocolVersion required");
            };
            json!({"protocolVersion":if VERSIONS.contains(&requested){requested}else{"2025-06-18"},"capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"cangling-map","version":env!("CARGO_PKG_VERSION")},"instructions":"只读地图数据服务。相对tilesetPath按MCP地址所属服务根目录解析；MAP_PUBLIC_BASE_URL配置后提供绝对tilesetUrl。加载和相机操作由客户端Earth完成。"})
        }
        "ping" => json!({}),
        "tools/list" => {
            if params.get("cursor").is_some() {
                return rpc_error(id, -32602, "This catalog has no pagination cursor");
            };
            json!({"tools":tools()})
        }
        "tools/call" => {
            let Some(name) = params["name"].as_str() else {
                return rpc_error(id, -32602, "Tool name required");
            };
            if !matches!(name, "search_3d_datasets" | "get_3d_dataset") {
                return rpc_error(id, -32602, "Unknown tool");
            }
            let q = match query(name, params.get("arguments").unwrap_or(&json!({}))) {
                Ok(q) => q,
                Err(e) => return rpc_error(id, -32602, &e),
            };
            match tiles3d::discover(State(state), Query(q)).await {
                Ok(Json(mut data)) => {
                    if name == "get_3d_dataset"
                        && data["datasets"].as_array().is_none_or(Vec::is_empty)
                    {
                        json!({"content":[{"type":"text","text":"数据集不存在、未发布或已停用"}],"isError":true})
                    } else {
                        if let Err(e) = public_urls(&mut data) {
                            return rpc_error(id, -32603, &e);
                        };
                        while data.to_string().len() > 30000 {
                            if let Some(items) = data["datasets"].as_array_mut() {
                                if items.pop().is_none() {
                                    break;
                                }
                            }
                            data["truncated"] = json!(true);
                        }
                        json!({"content":[{"type":"text","text":data.to_string()}],"structuredContent":data,"isError":false})
                    }
                }
                Err((status, _)) => {
                    json!({"content":[{"type":"text","text":if status==StatusCode::BAD_REQUEST{"查询范围无效"}else{"数据集查询失败，请检查服务状态"}}],"isError":true})
                }
            }
        }
        _ => return rpc_error(id, -32601, "Method not found"),
    };
    Json(json!({"jsonrpc":"2.0","id":id,"result":result})).into_response()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn schema_and_arguments() {
        assert_eq!(tools().as_array().unwrap().len(), 2);
        assert!(query("get_3d_dataset", &json!({"datasetId":"../secret"})).is_err());
        assert!(query("search_3d_datasets", &json!({"limit":0})).is_err());
        assert!(query(
            "search_3d_datasets",
            &json!({"query":"郑州","bounds":[113,34,114,35]})
        )
        .is_ok());
    }
    #[test]
    fn native_origin_and_browser_rejection() {
        assert!(origin_allowed(&HeaderMap::new()));
        let mut h = HeaderMap::new();
        h.insert("origin", "https://untrusted.invalid".parse().unwrap());
        assert!(!origin_allowed(&h));
    }
}
