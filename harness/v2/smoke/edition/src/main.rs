fn main(cx: &Cx, out: Out) -> Result<(), Error> {
    out.line(cx, "smoke-edition: bstd built under blaze")?;
    Ok(())
}
