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
