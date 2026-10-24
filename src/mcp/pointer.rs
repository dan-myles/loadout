use super::Error;

pub(super) fn parse(pointer: &str) -> Result<Vec<String>, Error> {
    let invalid = || Error::Pointer {
        pointer: pointer.to_owned(),
    };
    let rest = pointer.strip_prefix('/').ok_or_else(invalid)?;
    rest.split('/')
        .map(|part| {
            let mut name = String::new();
            let mut chars = part.chars();
            while let Some(c) = chars.next() {
                if c != '~' {
                    name.push(c);
                    continue;
                }
                match chars.next() {
                    Some('0') => name.push('~'),
                    Some('1') => name.push('/'),
                    _ => return Err(invalid()),
                }
            }
            if name.is_empty() {
                Err(invalid())
            } else {
                Ok(name)
            }
        })
        .collect()
}

pub(super) fn prefix(parts: &[String]) -> String {
    parts.iter().fold(String::new(), |mut out, part| {
        out.push('/');
        out.push_str(&part.replace('~', "~0").replace('/', "~1"));
        out
    })
}
