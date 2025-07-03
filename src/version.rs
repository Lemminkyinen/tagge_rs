use semver::Version;

pub trait ToVString {
    fn to_v_string(&self) -> String;
}

impl ToVString for Version {
    fn to_v_string(&self) -> String {
        format!("v{self}")
    }
}
