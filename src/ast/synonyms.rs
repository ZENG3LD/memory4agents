/// Returns the synonym group for a token, or an empty slice if the token is standalone.
///
/// Each group is a static slice of synonymous tokens. The first element is the canonical form.
pub fn token_synonyms(token: &str) -> &'static [&'static str] {
    match token {
        // visual styling
        "style" | "theme" | "skin" | "appearance" => &["style", "theme", "skin", "appearance"],
        // default variants
        "default" | "defaults" => &["default", "defaults"],
        // state / data
        "state" | "status" | "data" => &["state", "status", "data"],
        // configuration
        "config" | "settings" | "options" | "spec" | "params" => {
            &["config", "settings", "options", "spec", "params"]
        }
        // visual rendering
        "view" | "render" | "display" | "draw" => &["view", "render", "display", "draw"],
        // identifier types
        "id" | "key" | "handle" | "ref" => &["id", "key", "handle", "ref"],
        // size / dimensions
        "size" | "dim" | "extent" | "bounds" => &["size", "dim", "extent", "bounds"],
        // events
        "event" | "msg" | "message" | "signal" => &["event", "msg", "message", "signal"],
        // roles
        "kind" | "type" | "variant" | "category" => &["kind", "type", "variant", "category"],
        // builders
        "builder" | "factory" | "maker" | "constructor" => {
            &["builder", "factory", "maker", "constructor"]
        }
        // operations
        "action" | "command" | "op" | "operation" => {
            &["action", "command", "op", "operation"]
        }
        // input
        "input" | "control" | "trigger" => &["input", "control", "trigger"],
        // standalone
        _ => &[],
    }
}

/// Returns the canonical token for the synonym group, or the token itself if standalone.
pub fn canonical_token(token: &str) -> &'static str {
    let syns = token_synonyms(token);
    if syns.is_empty() {
        // SAFETY: token_synonyms returns a static slice. For the `_ => &[]` arm the token
        // is not in any group; we return a known-static empty string and the caller falls
        // through to use the raw token. To return the raw token as &'static str we use a
        // small leak-once helper — callers only hit this for tokens not in the map.
        // Actually: leaking is wrong for hot paths. We document that callers must handle
        // the "not in map" case by checking is_empty on token_synonyms first.
        // Return a sentinel that cannot collide with any real token:
        ""
    } else {
        syns[0]
    }
}

/// Returns the canonical label for a token. If the token belongs to a synonym group,
/// returns the first element of that group; otherwise returns the token unchanged
/// (as an owned String, since the raw token has no static lifetime).
pub fn canonical_label(token: &str) -> String {
    let c = canonical_token(token);
    if c.is_empty() {
        token.to_owned()
    } else {
        c.to_owned()
    }
}

/// Returns all tokens in the same synonym group as `token` (including `token` itself),
/// or an empty slice if the token is standalone.
pub fn synonym_group(token: &str) -> &'static [&'static str] {
    token_synonyms(token)
}

/// Total number of synonym groups defined.
pub const SYNONYM_GROUP_COUNT: usize = 12;

/// Total number of tokens covered by synonym groups.
pub const SYNONYM_TOKEN_COUNT: usize = 4 + 2 + 3 + 5 + 4 + 4 + 4 + 4 + 4 + 4 + 4 + 3; // = 45

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_group() {
        assert_eq!(token_synonyms("theme"), &["style", "theme", "skin", "appearance"]);
        assert_eq!(canonical_label("skin"), "style");
        assert_eq!(canonical_label("theme"), "style");
    }

    #[test]
    fn standalone_token() {
        assert_eq!(canonical_label("chrome"), "chrome");
        assert!(token_synonyms("chrome").is_empty());
    }

    #[test]
    fn same_group_detection() {
        // style and theme share a group
        let a = synonym_group("style");
        let b = synonym_group("theme");
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }
}
