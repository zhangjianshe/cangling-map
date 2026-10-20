use std::collections::HashMap;

use map_core::BoundingBox;
use thiserror::Error;

pub const WMS_VERSION: &str = "1.3.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operation {
    GetCapabilities,
    GetMap,
    GetFeatureInfo,
}

#[derive(Debug, Clone)]
pub struct WmsRequest {
    pub version: Option<String>,
    pub operation: Operation,
    pub parameters: HashMap<String, String>,
}

#[derive(Debug, Error)]
pub enum WmsError {
    #[error("missing required parameter: {0}")]
    MissingParameter(&'static str),
    #[error("unsupported operation: {0}")]
    OperationNotSupported(String),
}

impl WmsRequest {
    pub fn from_query(parameters: HashMap<String, String>) -> Result<Self, WmsError> {
        let parameters = parameters
            .into_iter()
            .map(|(key, value)| (key.to_ascii_uppercase(), value))
            .collect::<HashMap<_, _>>();

        let request = parameters
            .get("REQUEST")
            .ok_or(WmsError::MissingParameter("REQUEST"))?;

        let operation = match request.to_ascii_lowercase().as_str() {
            "getcapabilities" => Operation::GetCapabilities,
            "getmap" => Operation::GetMap,
            "getfeatureinfo" => Operation::GetFeatureInfo,
            _ => return Err(WmsError::OperationNotSupported(request.clone())),
        };

        Ok(Self {
            version: parameters.get("VERSION").cloned(),
            operation,
            parameters,
        })
    }
}

pub fn service_exception(code: &str, message: &str) -> String {
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ServiceExceptionReport version="1.3.0"
 xmlns="http://www.opengis.net/ogc"
 xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <ServiceException code="{code}">{escaped}</ServiceException>
</ServiceExceptionReport>"#
    )
}

#[derive(Debug, Clone)]
pub struct CapabilitiesLayer {
    pub id: String,
    pub name: String,
    pub title: String,
    pub abstract_text: Option<String>,
    pub queryable: bool,
    pub native_crs: String,
    pub native_extent: BoundingBox,
    pub supported_crs: Vec<String>,
    pub min_scale_denominator: Option<f64>,
    pub max_scale_denominator: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct CapabilitiesGroup {
    pub name: String,
    pub title: String,
    pub abstract_text: Option<String>,
    pub layers: Vec<CapabilitiesLayer>,
}

pub fn capabilities_xml(
    service_url: &str,
    layers: &[CapabilitiesLayer],
    groups: &[CapabilitiesGroup],
) -> String {
    let url = xml_escape(service_url);
    let mut layer_xml = String::new();
    for layer in layers {
        write_layer(&mut layer_xml, layer);
    }
    for group in groups {
        layer_xml.push_str(&format!(
            "<Layer><Name>{}</Name><Title>{}</Title>",
            xml_escape(&group.name),
            xml_escape(&group.title)
        ));
        if let Some(text) = &group.abstract_text {
            layer_xml.push_str(&format!("<Abstract>{}</Abstract>", xml_escape(text)));
        }
        for layer in &group.layers {
            write_layer(&mut layer_xml, layer);
        }
        layer_xml.push_str("</Layer>");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<WMS_Capabilities version="1.3.0" xmlns="http://www.opengis.net/wms" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.opengis.net/wms https://schemas.opengis.net/wms/1.3.0/capabilities_1_3_0.xsd">
<Service><Name>WMS</Name><Title>Cangling Map Server</Title><Abstract>OGC Web Map Service</Abstract><OnlineResource xlink:type="simple" xlink:href="{url}"/></Service>
<Capability><Request><GetCapabilities><Format>text/xml</Format><DCPType><HTTP><Get><OnlineResource xlink:type="simple" xlink:href="{url}"/></Get></HTTP></DCPType></GetCapabilities><GetMap><Format>image/png</Format><Format>image/jpeg</Format><DCPType><HTTP><Get><OnlineResource xlink:type="simple" xlink:href="{url}"/></Get></HTTP></DCPType></GetMap></Request><Exception><Format>XML</Format></Exception><Layer><Title>Cangling Map Server</Title>{layer_xml}</Layer></Capability></WMS_Capabilities>"#
    )
}

fn write_layer(xml: &mut String, layer: &CapabilitiesLayer) {
    let queryable = if layer.queryable { "1" } else { "0" };
    xml.push_str(&format!(
        "<Layer queryable=\"{queryable}\"><Name>{}</Name><Title>{}</Title>",
        xml_escape(&layer.name),
        xml_escape(&layer.title)
    ));
    if let Some(text) = &layer.abstract_text {
        xml.push_str(&format!("<Abstract>{}</Abstract>", xml_escape(text)));
    }
    if let Some(scale) = layer.min_scale_denominator {
        xml.push_str(&format!(
            "<MinScaleDenominator>{scale}</MinScaleDenominator>"
        ));
    }
    if let Some(scale) = layer.max_scale_denominator {
        xml.push_str(&format!(
            "<MaxScaleDenominator>{scale}</MaxScaleDenominator>"
        ));
    }
    let mut crs_values = layer.supported_crs.clone();
    if !crs_values.contains(&layer.native_crs) {
        crs_values.push(layer.native_crs.clone());
    }
    crs_values.sort();
    crs_values.dedup();
    for crs in crs_values {
        xml.push_str(&format!("<CRS>{}</CRS>", xml_escape(&crs)));
    }
    let b = layer.native_extent;
    if layer.native_crs.eq_ignore_ascii_case("EPSG:4326")
        || layer.native_crs.eq_ignore_ascii_case("CRS:84")
    {
        xml.push_str(&format!("<EX_GeographicBoundingBox><westBoundLongitude>{}</westBoundLongitude><eastBoundLongitude>{}</eastBoundLongitude><southBoundLatitude>{}</southBoundLatitude><northBoundLatitude>{}</northBoundLatitude></EX_GeographicBoundingBox>",b.min_x,b.max_x,b.min_y,b.max_y));
    }
    if layer.native_crs.eq_ignore_ascii_case("EPSG:4326") {
        xml.push_str(&format!(
            "<BoundingBox CRS=\"EPSG:4326\" minx=\"{}\" miny=\"{}\" maxx=\"{}\" maxy=\"{}\"/>",
            b.min_y, b.min_x, b.max_y, b.max_x
        ));
    } else {
        xml.push_str(&format!(
            "<BoundingBox CRS=\"{}\" minx=\"{}\" miny=\"{}\" maxx=\"{}\" maxy=\"{}\"/>",
            xml_escape(&layer.native_crs),
            b.min_x,
            b.min_y,
            b.max_x,
            b.max_y
        ));
    }
    xml.push_str("</Layer>");
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_names_are_case_insensitive() {
        let request = WmsRequest::from_query(HashMap::from([
            ("service".into(), "WMS".into()),
            ("ReQuEsT".into(), "GetMap".into()),
        ]))
        .expect("valid request");

        assert_eq!(request.operation, Operation::GetMap);
    }

    #[test]
    fn capabilities_uses_epsg_4326_axis_order() {
        let xml = capabilities_xml(
            "http://localhost/wms",
            &[CapabilitiesLayer {
                id: "layer-1".into(),
                name: "terrain".into(),
                title: "Terrain".into(),
                abstract_text: None,
                queryable: false,
                native_crs: "EPSG:4326".into(),
                native_extent: BoundingBox {
                    min_x: 103.,
                    min_y: 35.,
                    max_x: 104.,
                    max_y: 36.,
                },
                supported_crs: vec![],
                min_scale_denominator: Some(1_000.0),
                max_scale_denominator: Some(1_000_000.0),
            }],
            &[],
        );
        assert!(xml.contains("<Name>terrain</Name>"));
        assert!(xml.contains("<MinScaleDenominator>1000</MinScaleDenominator>"));
        assert!(xml.contains("<MaxScaleDenominator>1000000</MaxScaleDenominator>"));
        assert!(xml.contains("minx=\"35\" miny=\"103\" maxx=\"36\" maxy=\"104\""));
    }

    #[test]
    fn capabilities_nests_layers_in_directory_groups() {
        let layer = CapabilitiesLayer {
            id: "terrain-id".into(),
            name: "terrain".into(),
            title: "区域地形".into(),
            abstract_text: None,
            queryable: false,
            native_crs: "EPSG:4326".into(),
            native_extent: BoundingBox {
                min_x: 103.,
                min_y: 35.,
                max_x: 104.,
                max_y: 36.,
            },
            supported_crs: vec![],
            min_scale_denominator: None,
            max_scale_denominator: None,
        };
        let xml = capabilities_xml(
            "http://localhost/wms",
            &[],
            &[CapabilitiesGroup {
                name: "base".into(),
                title: "基础数据".into(),
                abstract_text: Some("底图目录".into()),
                layers: vec![layer],
            }],
        );
        assert!(xml.contains(
            "<Layer><Name>base</Name><Title>基础数据</Title><Abstract>底图目录</Abstract><Layer queryable=\"0\"><Name>terrain</Name>"
        ));
    }
}
