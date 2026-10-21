use std::{
    fs::{File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use map_gpu::ByteLruCache;
use map_render::RasterizedRgba;
use serde::Serialize;
use tracing::warn;

const MAGIC: [u8; 8] = *b"CLRGBA01";
const FORMAT_VERSION: u32 = 1;
const HEADER_BYTES: u64 = 8 + 4 + 8 + 4 + 4 + 8;
static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);
static CACHE: OnceLock<CpuRenderCache> = OnceLock::new();

pub(crate) fn global() -> &'static CpuRenderCache {
    CACHE.get_or_init(CpuRenderCache::from_environment)
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CpuRenderCacheStats {
    pub enabled: bool,
    pub memory_entries: usize,
    pub memory_resident_bytes: usize,
    pub memory_hits: u64,
    pub disk_hits: u64,
    pub misses: u64,
    pub writes: u64,
    pub errors: u64,
}

pub(crate) struct CpuRenderCache {
    root: Option<PathBuf>,
    max_entry_bytes: u64,
    memory: Mutex<ByteLruCache<u64, Arc<RasterizedRgba>>>,
    memory_hits: AtomicU64,
    disk_hits: AtomicU64,
    misses: AtomicU64,
    writes: AtomicU64,
    errors: AtomicU64,
}

impl CpuRenderCache {
    fn from_environment() -> Self {
        let root = std::env::var_os("MAP_CPU_RENDER_CACHE_PATH")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("MAP_CACHE_ROOT")
                    .filter(|path| !path.is_empty())
                    .map(PathBuf::from)
                    .map(|path| path.join("cpu-render"))
            })
            .or_else(|| Some(PathBuf::from("cache/cpu-render")))
            .filter(|path| !path.as_os_str().is_empty());
        let memory_mb = environment_megabytes("MAP_CPU_RENDER_CACHE_MB", 512);
        let max_entry_mb = environment_megabytes("MAP_CPU_RENDER_CACHE_MAX_ENTRY_MB", 512) as u64;
        Self {
            root,
            max_entry_bytes: max_entry_mb.saturating_mul(1024 * 1024),
            memory: Mutex::new(ByteLruCache::new(memory_mb.saturating_mul(1024 * 1024))),
            memory_hits: AtomicU64::new(0),
            disk_hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            writes: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }

    pub(crate) fn load(
        &self,
        key: u64,
        expected_width: u32,
        expected_height: u32,
    ) -> Option<Arc<RasterizedRgba>> {
        if let Some(cached) = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned()
        {
            self.memory_hits.fetch_add(1, Ordering::Relaxed);
            return Some(cached);
        }
        let Some(path) = self.path(key) else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        match read_entry(
            &path,
            key,
            expected_width,
            expected_height,
            self.max_entry_bytes,
        ) {
            Ok(Some(rendered)) => {
                self.disk_hits.fetch_add(1, Ordering::Relaxed);
                let rendered = Arc::new(rendered);
                self.insert_memory(key, rendered.clone());
                Some(rendered)
            }
            Ok(None) => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
            Err(error) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
                self.misses.fetch_add(1, Ordering::Relaxed);
                let _ = std::fs::remove_file(&path);
                warn!(key, %error, "discarded unreadable CPU render cache entry");
                None
            }
        }
    }

    pub(crate) fn store(&self, key: u64, rendered: RasterizedRgba) {
        let rendered = Arc::new(rendered);
        if let Some(path) = self.path(key) {
            match write_entry_atomic(&path, key, &rendered, self.max_entry_bytes) {
                Ok(()) => {
                    self.writes.fetch_add(1, Ordering::Relaxed);
                }
                Err(error) => {
                    self.errors.fetch_add(1, Ordering::Relaxed);
                    warn!(key, %error, "failed to persist CPU render cache entry");
                }
            }
        }
        self.insert_memory(key, rendered);
    }

    pub(crate) fn stats(&self) -> CpuRenderCacheStats {
        let memory = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        CpuRenderCacheStats {
            enabled: self.root.is_some(),
            memory_entries: memory.len(),
            memory_resident_bytes: memory.resident_bytes(),
            memory_hits: self.memory_hits.load(Ordering::Relaxed),
            disk_hits: self.disk_hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    fn insert_memory(&self, key: u64, rendered: Arc<RasterizedRgba>) {
        let bytes = rendered.pixels.len();
        let _ = self
            .memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key, rendered, bytes);
    }

    fn path(&self, key: u64) -> Option<PathBuf> {
        self.root
            .as_ref()
            .map(|root| root.join(format!("v{FORMAT_VERSION}-{key:016x}.crgba")))
    }
}

fn environment_megabytes(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn write_entry_atomic(
    path: &Path,
    key: u64,
    rendered: &RasterizedRgba,
    max_entry_bytes: u64,
) -> io::Result<()> {
    let expected = expected_rgba_bytes(rendered.width, rendered.height)?;
    if rendered.pixels.len() as u64 != expected {
        return Err(invalid_data("CPU render cache RGBA length is invalid"));
    }
    let entry_bytes = HEADER_BYTES
        .checked_add(expected)
        .ok_or_else(|| invalid_data("CPU render cache size overflows"))?;
    if entry_bytes > max_entry_bytes {
        return Err(invalid_data(format!(
            "CPU render cache entry is {entry_bytes} bytes, above the configured limit {max_entry_bytes}"
        )));
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid_data("CPU render cache path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = path.with_extension(format!("crgba.tmp-{}-{id}", std::process::id()));
    let result = (|| {
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(&MAGIC)?;
        write_u32(&mut writer, FORMAT_VERSION)?;
        write_u64(&mut writer, key)?;
        write_u32(&mut writer, rendered.width)?;
        write_u32(&mut writer, rendered.height)?;
        write_u64(&mut writer, expected)?;
        writer.write_all(&rendered.pixels)?;
        writer.flush()?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn read_entry(
    path: &Path,
    expected_key: u64,
    expected_width: u32,
    expected_height: u32,
    max_entry_bytes: u64,
) -> io::Result<Option<RasterizedRgba>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let file_bytes = file.metadata()?.len();
    if file_bytes > max_entry_bytes {
        return Err(invalid_data(format!(
            "CPU render cache entry is {file_bytes} bytes, above the configured limit {max_entry_bytes}"
        )));
    }
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; 8];
    reader.read_exact(&mut magic)?;
    if magic != MAGIC || read_u32(&mut reader)? != FORMAT_VERSION {
        return Err(invalid_data("unsupported CPU render cache format"));
    }
    if read_u64(&mut reader)? != expected_key {
        return Err(invalid_data("CPU render cache key does not match"));
    }
    let width = read_u32(&mut reader)?;
    let height = read_u32(&mut reader)?;
    if width != expected_width || height != expected_height {
        return Err(invalid_data("CPU render cache dimensions do not match"));
    }
    let rgba_bytes = read_u64(&mut reader)?;
    if rgba_bytes != expected_rgba_bytes(width, height)?
        || file_bytes != HEADER_BYTES.saturating_add(rgba_bytes)
    {
        return Err(invalid_data("CPU render cache byte length is invalid"));
    }
    let rgba_len = usize::try_from(rgba_bytes)
        .map_err(|_| invalid_data("CPU render cache RGBA length is too large"))?;
    let mut pixels = vec![0u8; rgba_len];
    reader.read_exact(&mut pixels)?;
    Ok(Some(RasterizedRgba {
        width,
        height,
        pixels,
    }))
}

fn expected_rgba_bytes(width: u32, height: u32) -> io::Result<u64> {
    u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| invalid_data("CPU render cache dimensions overflow"))
}

fn write_u32(writer: &mut impl Write, value: u32) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> io::Result<()> {
    writer.write_all(&value.to_le_bytes())
}

fn read_u32(reader: &mut impl Read) -> io::Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> io::Result<u64> {
    let mut bytes = [0; 8];
    reader.read_exact(&mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered() -> RasterizedRgba {
        RasterizedRgba {
            width: 2,
            height: 2,
            pixels: (0..16).collect(),
        }
    }

    fn test_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cangling-map-{name}-{}-{}.crgba",
            std::process::id(),
            TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn round_trips_cpu_render_cache() {
        let path = test_path("cpu-round-trip");
        let expected = rendered();
        write_entry_atomic(&path, 7, &expected, 1024).unwrap();
        let actual = read_entry(&path, 7, 2, 2, 1024).unwrap().unwrap();
        assert_eq!(actual, expected);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_truncated_cpu_render_cache() {
        let path = test_path("cpu-truncated");
        write_entry_atomic(&path, 7, &rendered(), 1024).unwrap();
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(20)
            .unwrap();
        assert!(read_entry(&path, 7, 2, 2, 1024).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn refuses_to_publish_oversized_cpu_render_cache() {
        let path = test_path("cpu-oversized");
        let error = write_entry_atomic(&path, 7, &rendered(), 32).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!path.exists());
    }
}
