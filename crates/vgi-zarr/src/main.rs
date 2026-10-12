//! Native VGI worker: `ATTACH 'zarr' (TYPE vgi, LOCATION './vgi-zarr')`.

fn main() {
    // stdout is the Arrow IPC channel and the extension swallows stderr, so
    // VGI_ZARR_LOG=<file> redirects logs (level from VGI_LOG, default warn).
    let mut b =
        env_logger::Builder::from_env(env_logger::Env::default().filter_or("VGI_LOG", "warn"));
    if let Ok(path) = std::env::var("VGI_ZARR_LOG") {
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            b.target(env_logger::Target::Pipe(Box::new(f)));
        }
    }
    let _ = b.try_init();
    vgi_zarr::build().run();
}
