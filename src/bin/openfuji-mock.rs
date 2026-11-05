//! Fake camera for testing: `openfuji-mock [ip] [pictures]`, then connect to that IP.
fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let mut args = std::env::args().skip(1);
    let ip = args.next().unwrap_or_else(|| "127.0.0.1".into()).parse()?;
    let pictures = args.next().map(|s| s.parse()).transpose()?.unwrap_or(12);
    openfuji::mock::MockCamera::new(pictures).serve(ip)
}
