/// Inserts the Host-resolved theme before the application body can paint.
pub fn inject_boot_theme(html: &str, preference: &str) -> String {
    let preference = match preference {
        "light" | "dark" | "system" => preference,
        _ => "system",
    };
    let script =
        format!(r#"<script src="/theme-bootstrap.js" data-preference="{preference}"></script>"#,);
    let at = html
        .find("<body")
        .and_then(|start| html[start..].find('>').map(|end| start + end + 1))
        .unwrap_or(html.len());
    format!("{}{}{}", &html[..at], script, &html[at..])
}

#[cfg(test)]
mod tests {
    use super::inject_boot_theme;

    #[test]
    fn injects_validated_preference_as_first_body_script() {
        let html = inject_boot_theme("<html><body></body></html>", "dark");
        assert!(html.contains("data-preference=\"dark\""));
        assert!(html.find("<body").unwrap() < html.find("<script").unwrap());
        assert!(html.find("<script").unwrap() < html.find("</body>").unwrap());
        assert!(inject_boot_theme("<body></body>", "bad").contains("data-preference=\"system\""));
    }
}
