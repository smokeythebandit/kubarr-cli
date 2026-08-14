use dialoguer::{Input, Password};

use crate::style::wizard_section;
use crate::types::AdminUser;
use crate::wizard_prompts::wizard_theme;

pub fn prompt_server_name(default: Option<&str>) -> Result<String, String> {
    wizard_section(
        "Server Identity",
        "Name this Kubarr instance so it is recognizable later.",
    );
    Input::with_theme(&wizard_theme())
        .with_prompt("Server name")
        .default(default.unwrap_or("Kubarr").to_string())
        .validate_with(|value: &String| -> Result<(), &str> {
            if value.trim().is_empty() {
                Err("server name is required")
            } else {
                Ok(())
            }
        })
        .interact_text()
        .map_err(|err| err.to_string())
}

pub fn prompt_admin_user(existing: Option<&AdminUser>) -> Result<AdminUser, String> {
    wizard_section(
        "Admin Account",
        "Create the first administrator for the Kubarr dashboard.",
    );
    let username = Input::with_theme(&wizard_theme())
        .with_prompt("Admin username")
        .default(
            existing
                .map(|admin| admin.username.as_str())
                .unwrap_or("admin")
                .to_string(),
        )
        .validate_with(required("admin username"))
        .interact_text()
        .map_err(|err| err.to_string())?;

    let email = Input::with_theme(&wizard_theme())
        .with_prompt("Admin email")
        .default(
            existing
                .map(|admin| admin.email.as_str())
                .unwrap_or("admin@kubarr.local")
                .to_string(),
        )
        .validate_with(required("admin email"))
        .interact_text()
        .map_err(|err| err.to_string())?;

    let password = Password::with_theme(&wizard_theme())
        .with_prompt("Admin password")
        .with_confirmation("Confirm admin password", "passwords do not match")
        .validate_with(|value: &String| -> Result<(), &str> {
            if value.len() < 8 {
                Err("password must be at least 8 characters")
            } else {
                Ok(())
            }
        })
        .interact()
        .map_err(|err| err.to_string())?;

    Ok(AdminUser {
        username,
        email,
        password,
    })
}

fn required(label: &'static str) -> impl Fn(&String) -> Result<(), String> {
    move |value: &String| {
        if value.trim().is_empty() {
            Err(format!("{label} is required"))
        } else {
            Ok(())
        }
    }
}
