pub mod error_class {
    use std::collections::HashMap;
    use std::fmt;
    use std::sync::Mutex;

    pub const CLASS: &'static str = "error_class";

    pub fn assert_class<T: fmt::Display>(value: T) -> T {
        panic!("Unexpected class: {}", value);
    }

    pub fn assert_unique<T: fmt::Display>(value: T) -> T {
        panic!("Duplicate class: {}", value);
    }

    pub fn assert_valid<T: fmt::Display>(value: T) -> T {
        panic!("Invalid class: {}", value);
    }

    pub fn assert_qualified<T: fmt::Display>(value: T) -> T {
        panic!("Invalid qualified class: {}", value);
    }
}
