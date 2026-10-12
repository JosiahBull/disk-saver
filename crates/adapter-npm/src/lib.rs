//! Clear npm's content-addressed download cache, preserving npx environments.
#![forbid(unsafe_code)]

/// The npm cache adapter factory.
pub fn factory() -> disk_saver_core::AdapterFactory {
    disk_saver_cachedir::native::factory(disk_saver_cachedir::native::Tool::Npm)
}
