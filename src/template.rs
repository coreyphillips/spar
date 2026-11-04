//! Filling prompt templates.

/// Fill each placeholder in `template` from `values`, in one pass.
///
/// A value goes in as it is and is never scanned again, so an issue that
/// mentions `{body}` reaches the agent saying `{body}`. Chained `str::replace`
/// calls cannot promise that, because each one rescans what the calls before it
/// inserted.
///
/// Names carry their braces, `("{title}", title)`, so searching for a
/// placeholder finds both the template and the call that fills it. Anything in
/// the template that no pair names is left alone.
pub fn fill(template: &str, values: &[(&str, &str)]) -> String {
    debug_assert!(
        values
            .iter()
            .all(|(name, _)| name.len() > 2 && name.starts_with('{') && name.ends_with('}')),
        "a placeholder name is written with its braces: {values:?}"
    );
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(at) = rest.find('{') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        match values.iter().find(|(name, _)| rest.starts_with(name)) {
            Some((name, value)) => {
                out.push_str(value);
                rest = &rest[name.len()..];
            }
            None => {
                out.push('{');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_naming_a_placeholder_is_not_filled_again() {
        let out = fill("{a} and {b}", &[("{a}", "says {b}"), ("{b}", "says {a}")]);
        assert_eq!("says {b} and says {a}", out);
    }

    #[test]
    fn every_occurrence_in_the_template_is_filled() {
        assert_eq!("1, 1", fill("{n}, {n}", &[("{n}", "1")]));
    }

    #[test]
    fn braces_that_name_nothing_are_kept() {
        assert_eq!(
            "{ x } {other} {n",
            fill("{ x } {other} {n", &[("{n}", "1")])
        );
        assert_eq!("{1}", fill("{{n}}", &[("{n}", "1")]));
    }

    #[test]
    fn text_around_a_placeholder_survives_multibyte_characters() {
        assert_eq!("é: ü.", fill("é: {v}.", &[("{v}", "ü")]));
    }
}
