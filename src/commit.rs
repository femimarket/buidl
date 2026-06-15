use anyhow::Result;

pub fn run() -> Result<()> {
    let message = buidl::lm()?;
    println!("{message}");
    Ok(())
}
