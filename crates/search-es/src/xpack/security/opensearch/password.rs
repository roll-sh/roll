//! The plugin's `PasswordValidator` with its default settings: no minimum length or regex,
//! at most 100 characters, and zxcvbn's score at least `strong` (3) with the user name as a user
//! input; a strong password built on the user name is "similar".

/// Why a password is refused (`None`: accepted).
pub fn check(user: &str, password: &str) -> Option<&'static str> {
    if password.is_empty() {
        return Some("Password is too short");
    }
    if password.chars().count() > 100 {
        return Some("Password is too long");
    }
    let strength = zxcvbn::zxcvbn(password, &[user]);
    if (strength.score() as u8) < 3 {
        return Some("Weak password");
    }
    let similar = strength.sequence().iter().any(|m| match &m.pattern {
        zxcvbn::matching::patterns::MatchPattern::Dictionary(d) => format!("{:?}", d.dictionary_name) == "UserInputs",
        _ => false,
    });
    similar.then_some("Password is similar to user name")
}

#[cfg(test)]
mod tests {
    use super::check;

    /// OpenSearch 3.9.0's answers for these passwords.
    #[test]
    fn strength_like_the_plugin() {
        for pw in ["abc", "abcdefgh", "Abcdefg1!", "esm-u9", " ", &"Zq9#".repeat(25)] {
            assert_eq!(check("esm-u9", pw), Some("Weak password"), "{pw}");
        }
        for pw in ["correcthorsebattery", "xK9#mQ2vLp", "Tr0ub4dor&3"] {
            assert_eq!(check("esm-u6", pw), None, "{pw}");
        }
        assert_eq!(check("esm-u1", "Esm-pass-1234!"), None);
        assert_eq!(check("esm-u6", "esm-u6-Strong-99!"), Some("Password is similar to user name"));
        assert_eq!(check("u", ""), Some("Password is too short"));
        assert_eq!(check("u", &format!("{}x", "Zq9#".repeat(25))), Some("Password is too long"));
    }
}
