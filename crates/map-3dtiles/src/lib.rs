//! Bounded validation and immutable publication of explicit 3D Tiles datasets.
//! Supported: nested tilesets, embedded-resource GLB, b3dm and pnts. No conversion.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const MAX_FILE: u64 = 256 * 1024 * 1024;
const MAX_TOTAL: u64 = 16 * 1024 * 1024 * 1024;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileInfo {
    pub bytes: u64,
    pub sha256: String,
    pub mime: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub revision: String,
    pub files: BTreeMap<String, FileInfo>,
    pub total_bytes: u64,
    pub tile_count: u64,
    pub bounding_volume: Value,
    pub asset_version: String,
}
pub fn valid_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err("ID 仅允许 1–64 位字母、数字、横线或下划线".into());
    }
    Ok(())
}
pub fn relative(path: &str) -> Result<(), String> {
    if path.is_empty()
        || path.len() > 1024
        || path
            .split('/')
            .any(|s| s.is_empty() || s == "." || s == ".." || s.starts_with('.'))
        || path.chars().any(|c| c.is_control() || ":\\%?#".contains(c))
    {
        return Err(format!("只支持无转义的本地相对路径：{path}"));
    }
    Ok(())
}
/// Reject symlinks in every component, including the configured data root.
pub fn safe_path(root: &Path, relative_path: &str) -> Result<PathBuf, String> {
    relative(relative_path)?;
    let full = root.join(relative_path);
    let absolute = if full.is_absolute() {
        full
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(full)
    };
    let mut part = PathBuf::new();
    for component in absolute.components() {
        part.push(component);
        let meta = fs::symlink_metadata(&part).map_err(|e| format!("{}: {e}", part.display()))?;
        if meta.file_type().is_symlink() {
            return Err("数据路径不允许符号链接".into());
        }
    }
    Ok(absolute)
}
struct Walker<'a> {
    source: &'a Path,
    destination: Option<&'a Path>,
    manifest: Manifest,
    visiting: BTreeSet<String>,
}
fn finite_array(v: &Value, n: usize) -> bool {
    v.as_array()
        .is_some_and(|a| a.len() == n && a.iter().all(|x| x.as_f64().is_some_and(f64::is_finite)))
}
fn volume(v: &Value) -> Result<(), String> {
    let mut count = 0;
    for (key, n) in [("box", 12), ("sphere", 4), ("region", 6)] {
        if v.get(key).is_some() {
            count += 1;
            if !finite_array(&v[key], n) {
                return Err("boundingVolume 无效".into());
            }
            if key == "sphere" && v[key][3].as_f64().unwrap() < 0. {
                return Err("球体半径必须非负".into());
            }
            if key == "region" {
                let a = v[key].as_array().unwrap();
                let x: Vec<f64> = a.iter().map(|v| v.as_f64().unwrap()).collect();
                if x[0].abs() > std::f64::consts::PI
                    || x[2].abs() > std::f64::consts::PI
                    || x[1].abs() > std::f64::consts::FRAC_PI_2
                    || x[3].abs() > std::f64::consts::FRAC_PI_2
                    || x[1] > x[3]
                    || x[4] > x[5]
                {
                    return Err("region 必须为弧度及米，南北/高度范围须有序".into());
                }
            }
        }
    }
    if count != 1 {
        return Err("boundingVolume 必须且只能包含 box、sphere、region 之一".into());
    }
    Ok(())
}
fn error_bound(v: &Value) -> Result<(), String> {
    if !v.as_f64().is_some_and(|n| n.is_finite() && n >= 0.) {
        return Err("geometricError 必须非负".into());
    }
    Ok(())
}
fn u32_at(data: &[u8], i: usize) -> Result<usize, String> {
    Ok(u32::from_le_bytes(
        data.get(i..i + 4)
            .ok_or("文件头被截断")?
            .try_into()
            .unwrap(),
    ) as usize)
}
fn glb(data: &[u8]) -> Result<(), String> {
    if data.get(..4) != Some(b"glTF") || u32_at(data, 4)? != 2 || u32_at(data, 8)? != data.len() {
        return Err("GLB 头或长度无效".into());
    }
    let len = u32_at(data, 12)?;
    if data.get(16..20) != Some(b"JSON") || len % 4 != 0 {
        return Err("GLB 缺少 JSON chunk".into());
    }
    let doc: Value = serde_json::from_slice(data.get(20..20 + len).ok_or("GLB JSON 截断")?)
        .map_err(|e| e.to_string())?;
    if doc["asset"]["version"] != "2.0" {
        return Err("仅支持 glTF 2.0".into());
    }
    for field in ["buffers", "images"] {
        if doc[field]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.get("uri").is_some()))
        {
            return Err("GLB 仅支持内嵌缓冲区和纹理".into());
        }
    }
    let mut pos = 20 + len;
    while pos < data.len() {
        let size = u32_at(data, pos)?;
        if size % 4 != 0 || pos + 8 + size > data.len() {
            return Err("GLB chunk 无效".into());
        }
        pos += 8 + size;
    }
    Ok(())
}
impl Walker<'_> {
    fn file(&mut self, path: &str, depth: usize) -> Result<(), String> {
        if depth > 64 {
            return Err("tileset 嵌套超过64层".into());
        }
        if self.visiting.contains(path) {
            return Err("tileset 存在循环引用".into());
        }
        if self.manifest.files.contains_key(path) {
            return Ok(());
        }
        if self.manifest.files.len() >= 100_000 {
            return Err("超过100000个文件".into());
        }
        let full = safe_path(self.source, path)?;
        let file = fs::File::open(full).map_err(|e| e.to_string())?;
        if !file.metadata().map_err(|e| e.to_string())?.is_file() {
            return Err("引用必须是普通文件".into());
        }
        let limit = if path.ends_with(".json") {
            16 * 1024 * 1024
        } else {
            MAX_FILE
        };
        let mut data = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())?;
        if data.len() as u64 > limit {
            return Err("单文件超过限制".into());
        }
        self.manifest.total_bytes += data.len() as u64;
        if self.manifest.total_bytes > MAX_TOTAL {
            return Err("数据集超过16GiB".into());
        }
        self.visiting.insert(path.into());
        let mime = if path.ends_with(".json") {
            let doc: Value = serde_json::from_slice(&data).map_err(|e| e.to_string())?;
            if !matches!(doc["asset"]["version"].as_str(), Some("1.0" | "1.1")) {
                return Err("tileset asset.version 必须为1.0或1.1".into());
            }
            if doc.get("schemaUri").is_some()
                || doc.get("extensions").is_some()
                || doc
                    .get("extensionsRequired")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty())
            {
                return Err("第一轮暂不支持外部元数据或必需扩展".into());
            }
            error_bound(&doc["geometricError"])?;
            if path == "tileset.json" {
                self.manifest.bounding_volume = doc["root"]["boundingVolume"].clone();
                self.manifest.asset_version = doc["asset"]["version"].as_str().unwrap().into();
            }
            self.tile(&doc["root"], path, depth + 1, true)?;
            "application/json"
        } else if path.ends_with(".glb") {
            glb(&data)?;
            "model/gltf-binary"
        } else if path.ends_with(".b3dm") || path.ends_with(".pnts") {
            let magic = if path.ends_with(".b3dm") {
                b"b3dm"
            } else {
                b"pnts"
            };
            if data.get(..4) != Some(magic)
                || u32_at(&data, 4)? != 1
                || u32_at(&data, 8)? != data.len()
            {
                return Err("传统瓦片头无效".into());
            }
            let offset = 28
                + (12..28)
                    .step_by(4)
                    .map(|i| u32_at(&data, i))
                    .collect::<Result<Vec<_>, _>>()?
                    .iter()
                    .sum::<usize>();
            if offset > data.len() {
                return Err("传统瓦片表长度无效".into());
            }
            let feature_len = u32_at(&data, 12)?;
            let feature: Value = serde_json::from_slice(
                data.get(28..28 + feature_len)
                    .ok_or("Feature Table JSON 被截断")?,
            )
            .map_err(|_| "Feature Table JSON 无效")?;
            let count_key = if magic == b"b3dm" {
                "BATCH_LENGTH"
            } else {
                "POINTS_LENGTH"
            };
            let count = feature[count_key]
                .as_u64()
                .ok_or("Feature Table 缺少有效数量")?;
            if magic == b"pnts" {
                let binary_len = u32_at(&data, 16)? as u64;
                let (key, stride) = if feature.get("POSITION").is_some() {
                    ("POSITION", 12)
                } else {
                    ("POSITION_QUANTIZED", 6)
                };
                let byte_offset = feature[key]["byteOffset"]
                    .as_u64()
                    .ok_or("pnts 缺少位置数据")?;
                if count
                    .checked_mul(stride)
                    .and_then(|n| n.checked_add(byte_offset))
                    .map_or(true, |n| n > binary_len)
                {
                    return Err("pnts 位置缓冲区被截断".into());
                }
                if key == "POSITION_QUANTIZED"
                    && (!finite_array(&feature["QUANTIZED_VOLUME_OFFSET"], 3)
                        || !finite_array(&feature["QUANTIZED_VOLUME_SCALE"], 3))
                {
                    return Err("pnts 缺少量化范围".into());
                }
            }
            if magic == b"b3dm" {
                glb(&data[offset..])?;
            } else if offset != data.len() {
                return Err("pnts 长度无效".into());
            }
            "application/octet-stream"
        } else {
            return Err(format!("暂不支持此内容格式：{path}"));
        };
        if let Some(dst) = self.destination {
            let out = dst.join(path);
            fs::create_dir_all(out.parent().unwrap()).map_err(|e| e.to_string())?;
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(out)
                .map_err(|e| e.to_string())?;
            file.write_all(&data).map_err(|e| e.to_string())?;
            file.sync_all().map_err(|e| e.to_string())?;
        }
        self.manifest.files.insert(
            path.into(),
            FileInfo {
                bytes: data.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&data)),
                mime: mime.into(),
            },
        );
        self.visiting.remove(path);
        Ok(())
    }
    fn tile(&mut self, tile: &Value, path: &str, depth: usize, root: bool) -> Result<(), String> {
        if depth > 64 {
            return Err("瓦片树超过64层".into());
        }
        self.manifest.tile_count += 1;
        if self.manifest.tile_count > 100_000 {
            return Err("超过100000个瓦片".into());
        }
        volume(&tile["boundingVolume"])?;
        error_bound(&tile["geometricError"])?;
        if tile.get("implicitTiling").is_some() || tile.get("extensions").is_some() {
            return Err("第一轮暂不支持隐式瓦片或瓦片扩展".into());
        }
        if tile.get("transform").is_some() && !finite_array(&tile["transform"], 16) {
            return Err("transform 必须为16个有限数值".into());
        }
        if (root || tile.get("refine").is_some())
            && !matches!(tile["refine"].as_str(), Some("ADD" | "REPLACE"))
        {
            return Err("根瓦片必须声明 refine: ADD 或 REPLACE".into());
        }
        let mut contents = vec![];
        if let Some(v) = tile.get("content") {
            contents.push(v);
        }
        if let Some(v) = tile.get("contents") {
            if !contents.is_empty() {
                return Err("content 与 contents 不可并存".into());
            }
            contents.extend(v.as_array().ok_or("contents 必须为数组")?);
        }
        for content in contents {
            let uri = content
                .get("uri")
                .or_else(|| content.get("url"))
                .and_then(Value::as_str)
                .ok_or("缺少 content.uri")?;
            relative(uri)?;
            let parent = Path::new(path).parent().unwrap();
            let relative_path = parent
                .join(uri)
                .to_str()
                .ok_or("路径不是UTF-8")?
                .replace('\\', "/");
            self.file(&relative_path, depth + 1)?;
        }
        if let Some(children) = tile.get("children") {
            for child in children.as_array().ok_or("children 必须为数组")? {
                self.tile(child, path, depth + 1, false)?;
            }
        }
        Ok(())
    }
}
/// Validation and copying use the same bytes; a failed operation never publishes partial files.
pub fn inspect(source: &Path, destination: Option<&Path>) -> Result<Manifest, String> {
    let mut walker = Walker {
        source,
        destination,
        visiting: BTreeSet::new(),
        manifest: Manifest {
            revision: String::new(),
            files: BTreeMap::new(),
            total_bytes: 0,
            tile_count: 0,
            bounding_volume: Value::Null,
            asset_version: String::new(),
        },
    };
    walker.file("tileset.json", 0)?;
    let mut hash = Sha256::new();
    for (path, file) in &walker.manifest.files {
        hash.update((path.len() as u64).to_le_bytes());
        hash.update(path.as_bytes());
        hash.update(file.sha256.as_bytes());
    }
    walker.manifest.revision = format!("{:x}", hash.finalize());
    Ok(walker.manifest)
}
pub fn publish(root: &Path, id: &str) -> Result<Manifest, String> {
    valid_id(id)?;
    let source = safe_path(root, &format!("{id}/source"))?;
    let versions = root.join(id).join("versions");
    fs::create_dir_all(&versions).map_err(|e| e.to_string())?;
    safe_path(root, &format!("{id}/versions"))?;
    let stage = versions.join(format!(".staging-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&stage).map_err(|e| e.to_string())?;
    let result = (|| {
        let manifest = inspect(&source, Some(&stage))?;
        let target = versions.join(&manifest.revision);
        if !target.exists() {
            fs::rename(&stage, target).map_err(|e| e.to_string())?;
        }
        Ok(manifest)
    })();
    if stage.exists() {
        let _ = fs::remove_dir_all(stage);
    }
    result
}
