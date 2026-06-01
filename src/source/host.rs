// SPDX-License-Identifier: EUPL-1.2

pub(super) fn is_gitlab(host: &str) -> bool {
    let lowered = host.to_lowercase();
    let name = without_port_for_classification(&lowered);
    name == "gitlab.com" || name.starts_with("gitlab.")
}

pub(super) fn normalized(host: &str) -> String {
    normalized_with_default_port(host, Some("443"))
}

pub(super) fn normalized_with_default_port(host: &str, default_port: Option<&str>) -> String {
    let lowered = host.to_lowercase();
    let (name, port) = split_port(&lowered);
    if port.is_some() && port == default_port {
        name.to_owned()
    } else {
        lowered
    }
}

fn split_port(host: &str) -> (&str, Option<&str>) {
    let Some((name, port)) = host.rsplit_once(':') else {
        return (host, None);
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return (host, None);
    }
    (name, Some(port))
}

fn without_port_for_classification(host: &str) -> &str {
    host.split_once(':').map_or(host, |(name, _port)| name)
}

#[cfg(test)]
mod tests {
    use super::{
        is_gitlab,
        normalized,
    };

    #[test]
    fn gitlab_hosts_ignore_case_and_port() {
        assert!(is_gitlab("gitlab.com"));
        assert!(is_gitlab("GitLab.Com:443"));
        assert!(is_gitlab("GitLab.Example.Com:8443"));
        assert!(!is_gitlab("not-gitlab.example.com"));
        assert!(!is_gitlab("example.com"));
    }

    #[test]
    fn normalized_host_preserves_non_default_ports() {
        assert_eq!(normalized("GitLab.Example.Com"), "gitlab.example.com");
        assert_eq!(normalized("GitLab.Example.Com:443"), "gitlab.example.com");
        assert_eq!(normalized("GitLab.Example.Com:80"), "gitlab.example.com:80");
        assert_eq!(
            normalized("GitLab.Example.Com:8443"),
            "gitlab.example.com:8443"
        );
    }

    #[test]
    fn scheme_normalization_uses_scheme_default_port() {
        assert_eq!(
            super::normalized_with_default_port("GitLab.Example.Com:80", Some("80")),
            "gitlab.example.com"
        );
        assert_eq!(
            super::normalized_with_default_port("GitLab.Example.Com:443", Some("443")),
            "gitlab.example.com"
        );
        assert_eq!(
            super::normalized_with_default_port("GitLab.Example.Com:80", Some("443")),
            "gitlab.example.com:80"
        );
    }
}
