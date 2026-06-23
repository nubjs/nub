pub fn is_workspace_spec(spec: &str) -> bool {
    spec.starts_with("workspace:")
}

pub fn is_catalog_spec(spec: &str) -> bool {
    spec.starts_with("catalog:")
}

pub fn is_npm_spec(spec: &str) -> bool {
    spec.starts_with("npm:")
}

pub fn is_jsr_spec(spec: &str) -> bool {
    spec.starts_with("jsr:")
}

pub fn is_file_spec(spec: &str) -> bool {
    spec.starts_with("file:")
}

pub fn is_link_spec(spec: &str) -> bool {
    spec.starts_with("link:")
}

pub fn split_name_spec(input: &str) -> (&str, Option<&str>) {
    if let Some(rest) = input.strip_prefix('@') {
        if let Some(slash) = rest.find('/') {
            let after_slash = &rest[slash + 1..];
            if let Some(at) = after_slash.find('@') {
                let name_end = 1 + slash + 1 + at;
                return (&input[..name_end], Some(&input[name_end + 1..]));
            }
        }
        return (input, None);
    }
    if let Some(at) = input.find('@') {
        return (&input[..at], Some(&input[at + 1..]));
    }
    (input, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_catalog_detect() {
        assert!(is_workspace_spec("workspace:*"));
        assert!(is_workspace_spec("workspace:^"));
        assert!(!is_workspace_spec("^1.0.0"));
        assert!(is_catalog_spec("catalog:"));
        assert!(is_catalog_spec("catalog:default"));
        assert!(!is_catalog_spec("cat:foo"));
    }

    #[test]
    fn split_plain_name() {
        assert_eq!(split_name_spec("lodash"), ("lodash", None));
        assert_eq!(split_name_spec("lodash@4.17.0"), ("lodash", Some("4.17.0")));
    }

    #[test]
    fn split_scoped_name() {
        assert_eq!(split_name_spec("@babel/core"), ("@babel/core", None));
        assert_eq!(
            split_name_spec("@babel/core@7.0.0"),
            ("@babel/core", Some("7.0.0"))
        );
    }

    #[test]
    fn split_empty_version_kept_as_some() {
        assert_eq!(split_name_spec("lodash@"), ("lodash", Some("")));
    }
}
