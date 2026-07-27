use std::io::{self, Read};

use ff_terminal::project_emoji_code;

fn replace_project_names(mut message: String, project_names: &[String]) -> String {
    for project_name in project_names {
        message = message.replace(project_name, project_emoji_code(project_name));
    }
    message
}

fn main() -> anyhow::Result<()> {
    let project_names = std::env::args().skip(1).collect::<Vec<_>>();
    let mut message = String::new();
    io::stdin().read_to_string(&mut message)?;
    print!("{}", replace_project_names(message, &project_names));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::replace_project_names;

    #[test]
    fn one_message_digest_uses_project_emojis() {
        let message = "forge-fleet: 2 ready\nHireFlow360: 1 building".to_owned();
        let projects = vec!["forge-fleet".to_owned(), "HireFlow360".to_owned()];

        assert_eq!(
            replace_project_names(message, &projects),
            "🚀: 2 ready\n💼: 1 building"
        );
    }

    #[test]
    fn one_message_digest_preserves_unmapped_projects() {
        let message = "other-project: idle".to_owned();
        let projects = vec!["other-project".to_owned()];

        assert_eq!(
            replace_project_names(message, &projects),
            "other-project: idle"
        );
    }
}
