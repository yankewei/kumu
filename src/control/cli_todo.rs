use crate::db::{TodoItem, TodoStatus};

use super::cli::{USAGE, call, caller_pane, flags, print_json, runtime_error, usage_error};
use super::protocol::Request;

pub(super) fn todo_cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("list" | "ls") => todo_list(&args[1..]),
        Some("show" | "get") => todo_show(&args[1..]),
        Some("add" | "new") => todo_add(&args[1..]),
        Some("update" | "edit") => todo_update(&args[1..]),
        Some("review") => todo_review(&args[1..]),
        Some("done" | "complete") => todo_done(&args[1..]),
        Some("remove" | "rm" | "delete") => todo_remove(&args[1..]),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            0
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

fn todo_list(args: &[String]) -> i32 {
    let mut status = None;
    let mut mine = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--mine" => mine = true,
            "--status" | "-s" => {
                let Some(raw) = iter.next() else {
                    return usage_error("--status needs todo, in_progress, or done");
                };
                let Some(parsed) = TodoStatus::parse(raw) else {
                    return usage_error(&format!(
                        "unknown status '{raw}': use todo, in_progress, or done"
                    ));
                };
                status = Some(parsed);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other => return usage_error(&format!("unknown option {other}")),
        }
    }
    let value = match call(&Request::TodoList {
        from: caller_pane(),
        status,
        mine,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    if json {
        let Some(todos) = value.get("todos").filter(|v| v.is_array()) else {
            return runtime_error("unexpected reply: missing todos array");
        };
        println!("{todos}");
        return 0;
    }
    let todos: Vec<TodoItem> = match serde_json::from_value(value["todos"].clone()) {
        Ok(todos) => todos,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if todos.is_empty() {
        println!("no todos");
        return 0;
    }
    for todo in &todos {
        println!("{}", todo_summary_line(todo));
    }
    0
}

fn todo_show(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let [raw_id] = rest.as_slice() else {
        return usage_error("give one todo id");
    };
    let Ok(id) = raw_id.parse::<i64>() else {
        return usage_error(&format!("invalid todo id '{raw_id}'"));
    };
    let value = match call(&Request::TodoShow {
        id,
        from: caller_pane(),
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    if !item.description.trim().is_empty() {
        println!("---");
        println!("{}", item.description.trim_end());
    }
    0
}

fn todo_add(args: &[String]) -> i32 {
    let mut title_words = Vec::new();
    let mut description = String::new();
    let mut status = TodoStatus::Todo;
    let mut bind = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--bind" => bind = true,
            "--desc" | "-d" => match iter.next() {
                Some(text) => description = text.clone(),
                None => return usage_error("--desc needs text"),
            },
            "--status" | "-s" => {
                let Some(raw) = iter.next() else {
                    return usage_error("--status needs todo, in_progress, or done");
                };
                let Some(parsed) = TodoStatus::parse(raw) else {
                    return usage_error(&format!(
                        "unknown status '{raw}': use todo, in_progress, or done"
                    ));
                };
                status = parsed;
            }
            "-h" | "--help" if title_words.is_empty() => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') && title_words.is_empty() => {
                return usage_error(&format!("unknown option {other}"));
            }
            other => title_words.push(other),
        }
    }
    let title = title_words.join(" ").trim().to_string();
    if title.is_empty() {
        return usage_error("give a title for the todo");
    }
    let value = match call(&Request::TodoAdd {
        from: caller_pane(),
        title,
        description,
        status,
        bind,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    0
}

fn todo_update(args: &[String]) -> i32 {
    let mut id = None;
    let mut title = None;
    let mut description = None;
    let mut status = None;
    let mut awaiting_review = None;
    let mut bind = false;
    let mut unbind = false;
    let mut json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--bind" => bind = true,
            "--unbind" => unbind = true,
            "--review" => awaiting_review = Some(true),
            "--no-review" => awaiting_review = Some(false),
            "--title" | "-t" => match iter.next() {
                Some(text) => title = Some(text.clone()),
                None => return usage_error("--title needs text"),
            },
            "--desc" | "-d" => match iter.next() {
                Some(text) => description = Some(text.clone()),
                None => return usage_error("--desc needs text"),
            },
            "--status" | "-s" => {
                let Some(raw) = iter.next() else {
                    return usage_error("--status needs todo, in_progress, or done");
                };
                let Some(parsed) = TodoStatus::parse(raw) else {
                    return usage_error(&format!(
                        "unknown status '{raw}': use todo, in_progress, or done"
                    ));
                };
                status = Some(parsed);
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return 0;
            }
            other if other.starts_with('-') => {
                return usage_error(&format!("unknown option {other}"));
            }
            other if id.is_none() => match other.parse::<i64>() {
                Ok(parsed) => id = Some(parsed),
                Err(_) => return usage_error(&format!("invalid todo id '{other}'")),
            },
            other => return usage_error(&format!("unexpected argument '{other}'")),
        }
    }
    let Some(id) = id else {
        return usage_error("give the todo id to update");
    };
    if bind && unbind {
        return usage_error("use either --bind or --unbind, not both");
    }
    let value = match call(&Request::TodoUpdate {
        id,
        from: caller_pane(),
        title,
        description,
        status,
        awaiting_review,
        bind,
        unbind,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    0
}

fn todo_review(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let [raw_id] = rest.as_slice() else {
        return usage_error("give one todo id");
    };
    let Ok(id) = raw_id.parse::<i64>() else {
        return usage_error(&format!("invalid todo id '{raw_id}'"));
    };
    let value = match call(&Request::TodoUpdate {
        id,
        from: caller_pane(),
        title: None,
        description: None,
        status: Some(TodoStatus::InProgress),
        awaiting_review: Some(true),
        bind: false,
        unbind: false,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    0
}

fn todo_done(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let [raw_id] = rest.as_slice() else {
        return usage_error("give one todo id");
    };
    let Ok(id) = raw_id.parse::<i64>() else {
        return usage_error(&format!("invalid todo id '{raw_id}'"));
    };
    let value = match call(&Request::TodoUpdate {
        id,
        from: caller_pane(),
        title: None,
        description: None,
        status: Some(TodoStatus::Done),
        awaiting_review: Some(false),
        bind: false,
        unbind: false,
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    let item: TodoItem = match serde_json::from_value(value) {
        Ok(item) => item,
        Err(error) => return runtime_error(&format!("unexpected reply: {error}")),
    };
    if json {
        return print_json(&item);
    }
    println!("{}", todo_summary_line(&item));
    0
}

fn todo_remove(args: &[String]) -> i32 {
    let (json, rest) = match flags(args) {
        Ok(parsed) => parsed,
        Err(code) => return code,
    };
    let [raw_id] = rest.as_slice() else {
        return usage_error("give one todo id");
    };
    let Ok(id) = raw_id.parse::<i64>() else {
        return usage_error(&format!("invalid todo id '{raw_id}'"));
    };
    let value = match call(&Request::TodoRemove {
        id,
        from: caller_pane(),
    }) {
        Ok(value) => value,
        Err(error) => return runtime_error(&error),
    };
    if json {
        println!("{value}");
        return 0;
    }
    println!("removed #{id}");
    0
}

fn todo_summary_line(todo: &TodoItem) -> String {
    let mut line = format!("#{} [{}]", todo.id, todo.status.as_str());
    if todo.awaiting_review {
        line.push_str(" [review]");
    }
    line.push_str(&format!(" {}", todo.title));
    if let Some(agent) = &todo.agent_id {
        line.push_str(&format!(" · {agent}"));
    }
    if let Some(pane) = todo.pane_id {
        line.push_str(&format!(" (pane {pane})"));
    }
    line
}
