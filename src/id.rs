/// `is_id` as a clap value parser, for names given on the command line.
pub fn parse(value: &str) -> Result<String, String> {
    if is_id(value) {
        Ok(value.to_owned())
    } else {
        Err("must be lowercase letters, digits, - or _, starting with a letter or digit".to_owned())
    }
}

pub fn is_id(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && value.len() <= 64
}
