fn main() -> anyhow::Result<()> {
    let code = rrss::run_cli()?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}
