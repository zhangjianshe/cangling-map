use std::{
    fs::{File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use bytemuck::{cast_slice, cast_slice_mut, Pod, Zeroable};
use map_gpu::{AtlasOverlay, AtlasVertex, MeshChunk, Vertex};

const MAGIC: [u8; 8] = *b"CLMESH01";
const FORMAT_VERSION: u32 = 1;
const ENDIAN_MARKER: u32 = 0x0102_0304;
static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct GpuMeshArchive {
    pub width: u32,
    pub height: u32,
    pub background: [u8; 4],
    pub chunks: Vec<MeshChunk>,
    pub atlases: Vec<AtlasOverlay>,
}

#[derive(Debug, Clone)]
pub(crate) struct GpuMeshDiskCache {
    root: Option<PathBuf>,
    max_entry_bytes: u64,
}

impl GpuMeshDiskCache {
    pub(crate) fn from_environment() -> Self {
        let root = std::env::var_os("MAP_GPU_MESH_CACHE_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("MAP_CACHE_ROOT")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .map(|path| path.join("gpu-mesh"))
            })
            .or_else(|| Some(PathBuf::from("cache/gpu-mesh")))
            .filter(|path| !path.as_os_str().is_empty());
        let max_entry_mb = std::env::var("MAP_GPU_MESH_CACHE_MAX_ENTRY_MB")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(4096);
        Self {
            root,
            max_entry_bytes: max_entry_mb.saturating_mul(1024 * 1024),
        }
    }

    pub(crate) fn path(&self, key: u64) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join(format!("v{FORMAT_VERSION}-{key:016x}.cgmesh")))
    }

    pub(crate) fn is_enabled(&self) -> bool {
        self.root.is_some()
    }

    pub(crate) fn load(&self, key: u64) -> io::Result<Option<GpuMeshArchive>> {
        let Some(path) = self.path(key) else {
            return Ok(None);
        };
        if !path.is_file() {
            return Ok(None);
        }
        read_archive(&path, key, self.max_entry_bytes).map(Some)
    }

    pub(crate) fn store(&self, key: u64, archive: &GpuMeshArchive) -> io::Result<()> {
        let Some(path) = self.path(key) else {
            return Ok(());
        };
        write_archive_atomic(&path, key, archive, self.max_entry_bytes)
    }

    pub(crate) fn remove(&self, key: u64) -> io::Result<()> {
        let Some(path) = self.path(key) else {
            return Ok(());
        };
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn write_archive_atomic(
    path: &Path,
    key: u64,
    archive: &GpuMeshArchive,
    max_entry_bytes: u64,
) -> io::Result<()> {
    if !cfg!(target_endian = "little") {
        return Err(invalid_data(
            "GPU mesh cache only supports little-endian hosts",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("GPU mesh cache path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("cgmesh");
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("{extension}.tmp-{}-{id}", std::process::id()));
    let result = (|| {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&MAGIC)?;
        write_u32(&mut writer, FORMAT_VERSION)?;
        write_u32(&mut writer, ENDIAN_MARKER)?;
        write_u32(&mut writer, std::mem::size_of::<Vertex>() as u32)?;
        write_u32(&mut writer, std::mem::size_of::<AtlasVertex>() as u32)?;
        write_u64(&mut writer, key)?;
        write_u32(&mut writer, archive.width)?;
        write_u32(&mut writer, archive.height)?;
        writer.write_all(&archive.background)?;
        write_u32(&mut writer, len_u32(archive.chunks.len())?)?;
        write_u32(&mut writer, len_u32(archive.atlases.len())?)?;
        for chunk in &archive.chunks {
            write_u64(&mut writer, len_u64(chunk.vertices.len())?)?;
            write_u64(&mut writer, len_u64(chunk.indices.len())?)?;
            writer.write_all(cast_slice(&chunk.vertices))?;
            writer.write_all(cast_slice(&chunk.indices))?;
        }
        for atlas in &archive.atlases {
            write_u32(&mut writer, atlas.width)?;
            write_u32(&mut writer, atlas.height)?;
            writer.write_all(&[u8::from(atlas.repeat), u8::from(atlas.underlay), 0, 0])?;
            write_u64(&mut writer, len_u64(atlas.rgba.len())?)?;
            write_u64(&mut writer, len_u64(atlas.vertices.len())?)?;
            write_u64(&mut writer, len_u64(atlas.indices.len())?)?;
            writer.write_all(&atlas.rgba)?;
            writer.write_all(cast_slice(&atlas.vertices))?;
            writer.write_all(cast_slice(&atlas.indices))?;
        }
        writer.flush()?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        let file_bytes = file.metadata()?.len();
        if file_bytes > max_entry_bytes {
            return Err(invalid_data(format!(
                "GPU mesh cache entry is {file_bytes} bytes, above the configured limit {max_entry_bytes}"
            )));
        }
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

fn read_archive(
    path: &Path,
    expected_key: u64,
    max_entry_bytes: u64,
) -> io::Result<GpuMeshArchive> {
    if !cfg!(target_endian = "little") {
        return Err(invalid_data(
            "GPU mesh cache only supports little-endian hosts",
        ));
    }
    let file = File::open(path)?;
    let file_bytes = file.metadata()?.len();
    if file_bytes > max_entry_bytes {
        return Err(invalid_data(format!(
            "GPU mesh cache entry is {file_bytes} bytes, above the configured limit {max_entry_bytes}"
        )));
    }
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if magic != MAGIC {
        return Err(invalid_data("invalid GPU mesh cache magic"));
    }
    if read_u32(&mut reader)? != FORMAT_VERSION {
        return Err(invalid_data("unsupported GPU mesh cache version"));
    }
    if read_u32(&mut reader)? != ENDIAN_MARKER {
        return Err(invalid_data("GPU mesh cache endian marker does not match"));
    }
    if read_u32(&mut reader)? as usize != std::mem::size_of::<Vertex>()
        || read_u32(&mut reader)? as usize != std::mem::size_of::<AtlasVertex>()
    {
        return Err(invalid_data("GPU mesh cache vertex layout does not match"));
    }
    if read_u64(&mut reader)? != expected_key {
        return Err(invalid_data("GPU mesh cache key does not match"));
    }
    let width = read_u32(&mut reader)?;
    let height = read_u32(&mut reader)?;
    if width == 0 || height == 0 {
        return Err(invalid_data("GPU mesh cache dimensions must be non-zero"));
    }
    let mut background = [0u8; 4];
    reader.read_exact(&mut background)?;
    let chunk_count = bounded_count(read_u32(&mut reader)? as u64, 1, file_bytes)?;
    let atlas_count = bounded_count(read_u32(&mut reader)? as u64, 1, file_bytes)?;
    let mut chunks = Vec::with_capacity(chunk_count);
    for _ in 0..chunk_count {
        let vertex_count = bounded_count(
            read_u64(&mut reader)?,
            std::mem::size_of::<Vertex>(),
            file_bytes,
        )?;
        let index_count = bounded_count(
            read_u64(&mut reader)?,
            std::mem::size_of::<u32>(),
            file_bytes,
        )?;
        chunks.push(MeshChunk {
            vertices: read_pod_vec(&mut reader, vertex_count)?,
            indices: read_pod_vec(&mut reader, index_count)?,
        });
    }
    let mut atlases = Vec::with_capacity(atlas_count);
    for _ in 0..atlas_count {
        let atlas_width = read_u32(&mut reader)?;
        let atlas_height = read_u32(&mut reader)?;
        if atlas_width == 0 || atlas_height == 0 {
            return Err(invalid_data("GPU mesh atlas dimensions must be non-zero"));
        }
        let mut flags = [0u8; 4];
        reader.read_exact(&mut flags)?;
        if flags[0] > 1 || flags[1] > 1 {
            return Err(invalid_data("GPU mesh atlas flags are invalid"));
        }
        let rgba_len = bounded_count(read_u64(&mut reader)?, 1, file_bytes)?;
        let vertex_count = bounded_count(
            read_u64(&mut reader)?,
            std::mem::size_of::<AtlasVertex>(),
            file_bytes,
        )?;
        let index_count = bounded_count(
            read_u64(&mut reader)?,
            std::mem::size_of::<u32>(),
            file_bytes,
        )?;
        let expected_rgba = u64::from(atlas_width)
            .checked_mul(u64::from(atlas_height))
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or_else(|| invalid_data("GPU mesh atlas dimensions overflow"))?;
        if rgba_len as u64 != expected_rgba {
            return Err(invalid_data("GPU mesh atlas RGBA length is invalid"));
        }
        let mut rgba = vec![0u8; rgba_len];
        reader.read_exact(&mut rgba)?;
        atlases.push(AtlasOverlay {
            width: atlas_width,
            height: atlas_height,
            rgba,
            vertices: read_pod_vec(&mut reader, vertex_count)?,
            indices: read_pod_vec(&mut reader, index_count)?,
            repeat: flags[0] == 1,
            underlay: flags[1] == 1,
        });
    }
    let mut trailing = [0u8; 1];
    if reader.read(&mut trailing)? != 0 {
        return Err(invalid_data("GPU mesh cache entry has trailing data"));
    }
    Ok(GpuMeshArchive {
        width,
        height,
        background,
        chunks,
        atlases,
    })
}

fn read_pod_vec<T: Pod + Zeroable>(reader: &mut impl Read, count: usize) -> io::Result<Vec<T>> {
    let mut values = vec![T::zeroed(); count];
    reader.read_exact(cast_slice_mut(&mut values))?;
    Ok(values)
}

fn bounded_count(value: u64, element_bytes: usize, file_bytes: u64) -> io::Result<usize> {
    let count = usize::try_from(value).map_err(|_| invalid_data("GPU mesh count is too large"))?;
    let bytes = count
        .checked_mul(element_bytes)
        .ok_or_else(|| invalid_data("GPU mesh byte length overflows"))?;
    if bytes as u64 > file_bytes {
        return Err(invalid_data("GPU mesh count exceeds the cache file size"));
    }
    Ok(count)
}

fn len_u32(value: usize) -> io::Result<u32> {
    u32::try_from(value).map_err(|_| invalid_data("GPU mesh collection is too large"))
}

fn len_u64(value: usize) -> io::Result<u64> {
    u64::try_from(value).map_err(|_| invalid_data("GPU mesh collection is too large"))
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive() -> GpuMeshArchive {
        GpuMeshArchive {
            width: 64,
            height: 32,
            background: [1, 2, 3, 4],
            chunks: vec![MeshChunk {
                vertices: vec![Vertex {
                    position: [-1.0, 1.0, 0.0, 1.0],
                    color: [0.1, 0.2, 0.3, 0.4],
                }],
                indices: vec![0, 0, 0],
            }],
            atlases: vec![AtlasOverlay {
                width: 1,
                height: 1,
                rgba: vec![5, 6, 7, 8],
                vertices: vec![AtlasVertex {
                    position: [0.0, 0.0, 0.0, 1.0],
                    uv: [0.5, 0.5],
                    _padding: [0.0, 0.0],
                    color: [1.0; 4],
                }],
                indices: vec![0],
                repeat: true,
                underlay: false,
            }],
        }
    }

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cangling-map-{name}-{}-{}.cgmesh",
            std::process::id(),
            TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn round_trips_gpu_mesh_archive() {
        let path = test_path("round-trip");
        let expected = archive();
        write_archive_atomic(&path, 42, &expected, 1024 * 1024).unwrap();
        let actual = read_archive(&path, 42, 1024 * 1024).unwrap();
        assert_eq!(actual, expected);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_truncated_gpu_mesh_archive() {
        let path = test_path("truncated");
        let expected = archive();
        write_archive_atomic(&path, 42, &expected, 1024 * 1024).unwrap();
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(32).unwrap();
        assert!(read_archive(&path, 42, 1024 * 1024).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn refuses_to_publish_an_oversized_archive() {
        let path = test_path("oversized");
        let error = write_archive_atomic(&path, 42, &archive(), 32).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!path.exists());
    }
}
