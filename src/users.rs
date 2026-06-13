use std::process::Command;

use dialoguer::Password;

use crate::style::{ok, status_label, BLUE};
use crate::types::DATABASE_NAMESPACE;
use crate::util::{ensure_tool, has_help, next_value};
use crate::wizard_prompts::wizard_theme;

pub fn users(args: Vec<String>) {
    if has_help(&args) {
        print_help();
        return;
    }

    let mut iter = args.into_iter();
    let Some(command) = iter.next() else {
        print_help();
        return;
    };

    match command.as_str() {
        "list" => list_users(iter.collect()),
        "create-admin" => create_admin(iter.collect()),
        "reset-password" => reset_password(iter.collect()),
        "help" | "--help" | "-h" => print_help(),
        other => {
            eprintln!("unknown users command '{other}'");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        "Manage Kubarr users.\n\nUSAGE:\n    kubarr users <COMMAND> [OPTIONS]\n\nCOMMANDS:\n    list             List users and roles\n    create-admin     Create or update an admin user\n    reset-password   Reset a user's password\n\nOPTIONS:\n    --namespace <name>   Database namespace [default: kubarr-database]\n\nEXAMPLES:\n    kubarr users list\n    kubarr users create-admin --username admin --email admin@kubarr.local\n    kubarr users reset-password --username admin"
    );
}

fn list_users(args: Vec<String>) {
    let options = parse_common(args);
    run_psql(
        &options.namespace,
        "SELECT u.id, u.username, u.email, u.is_active, u.is_approved, COALESCE(string_agg(r.name, ',' ORDER BY r.name), '') AS roles FROM users u LEFT JOIN user_roles ur ON ur.user_id = u.id LEFT JOIN roles r ON r.id = ur.role_id GROUP BY u.id, u.username, u.email, u.is_active, u.is_approved ORDER BY u.id;",
    );
}

fn create_admin(args: Vec<String>) {
    let options = parse_user_options(args, true);
    let username = required(options.username, "--username");
    let email = required(options.email, "--email");
    let password = options.password.unwrap_or_else(prompt_password);
    let hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
        .unwrap_or_else(|err| panic!("failed to hash password: {err}"));

    run_psql(
        &options.namespace,
        &format!(
            "WITH admin_role AS (SELECT id FROM roles WHERE name='admin'), upserted_user AS (INSERT INTO users (username,email,hashed_password,is_active,is_approved,totp_enabled,created_at,updated_at) VALUES ('{}','{}','{}',TRUE,TRUE,FALSE,NOW(),NOW()) ON CONFLICT (username) DO UPDATE SET email=EXCLUDED.email, hashed_password=EXCLUDED.hashed_password, is_active=TRUE, is_approved=TRUE, updated_at=NOW() RETURNING id) INSERT INTO user_roles (user_id, role_id) SELECT upserted_user.id, admin_role.id FROM upserted_user, admin_role ON CONFLICT DO NOTHING;",
            sql_escape(&username),
            sql_escape(&email),
            sql_escape(&hash),
        ),
    );
    ok("admin user created or updated");
}

fn reset_password(args: Vec<String>) {
    let options = parse_user_options(args, false);
    let username = required(options.username, "--username");
    let password = options.password.unwrap_or_else(prompt_password);
    let hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
        .unwrap_or_else(|err| panic!("failed to hash password: {err}"));

    run_psql(
        &options.namespace,
        &format!(
            "UPDATE users SET hashed_password='{}', updated_at=NOW() WHERE username='{}';",
            sql_escape(&hash),
            sql_escape(&username),
        ),
    );
    ok("password reset");
}

struct CommonOptions {
    namespace: String,
}

struct UserOptions {
    namespace: String,
    username: Option<String>,
    email: Option<String>,
    password: Option<String>,
}

fn parse_common(args: Vec<String>) -> CommonOptions {
    let mut namespace = DATABASE_NAMESPACE.to_string();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--namespace" | "-n" => namespace = next_value(&mut iter, &arg).unwrap_or_exit(),
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => die(&format!("unknown option '{other}'")),
        }
    }
    CommonOptions { namespace }
}

fn parse_user_options(args: Vec<String>, allow_email: bool) -> UserOptions {
    let mut namespace = DATABASE_NAMESPACE.to_string();
    let mut username = None;
    let mut email = None;
    let mut password = None;
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--namespace" | "-n" => namespace = next_value(&mut iter, &arg).unwrap_or_exit(),
            "--username" => username = Some(next_value(&mut iter, &arg).unwrap_or_exit()),
            "--email" if allow_email => email = Some(next_value(&mut iter, &arg).unwrap_or_exit()),
            "--password" => password = Some(next_value(&mut iter, &arg).unwrap_or_exit()),
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other => die(&format!("unknown option '{other}'")),
        }
    }
    UserOptions {
        namespace,
        username,
        email,
        password,
    }
}

fn run_psql(namespace: &str, sql: &str) {
    ensure_tool("kubectl");
    println!(
        "    {} kubectl exec statefulset/kubarr-db -- psql",
        status_label("run", BLUE)
    );
    let status = Command::new("kubectl")
        .args([
            "exec",
            "-n",
            namespace,
            "statefulset/kubarr-db",
            "--",
            "psql",
            "-U",
            "kubarr",
            "-d",
            "kubarr",
            "-v",
            "ON_ERROR_STOP=1",
            "-P",
            "pager=off",
            "-c",
            sql,
        ])
        .status()
        .unwrap_or_else(|err| panic!("failed to run user SQL: {err}"));
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
}

fn prompt_password() -> String {
    Password::with_theme(&wizard_theme())
        .with_prompt("Password")
        .with_confirmation("Confirm password", "passwords do not match")
        .validate_with(|input: &String| {
            if input.len() >= 8 {
                Ok(())
            } else {
                Err("password must be at least 8 characters")
            }
        })
        .interact()
        .unwrap_or_else(|err| panic!("failed to read password: {err}"))
}

fn required(value: Option<String>, name: &str) -> String {
    value.unwrap_or_else(|| {
        die(&format!("missing required option {name}"));
    })
}

fn sql_escape(value: &str) -> String {
    value.replace('\'', "''")
}

fn die(message: &str) -> ! {
    eprintln!("{} {message}", status_label("error", crate::style::RED));
    std::process::exit(2);
}

trait UnwrapOrExit<T> {
    fn unwrap_or_exit(self) -> T;
}

impl<T> UnwrapOrExit<T> for Result<T, String> {
    fn unwrap_or_exit(self) -> T {
        match self {
            Ok(value) => value,
            Err(err) => die(&err),
        }
    }
}
