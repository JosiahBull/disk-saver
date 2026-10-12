//! Prune unused uv cache entries through uv's own locking and maintenance.
#![forbid(unsafe_code)]

/// The uv cache adapter factory.
pub fn factory() -> disk_saver_core::AdapterFactory {
    disk_saver_cachedir::native::factory(disk_saver_cachedir::native::Tool::Uv)
}
