use serde_json::Value;

pub fn bool_icon(v: bool) -> &'static str {
    if v { "[ON]" } else { "[OFF]" }
}

pub fn format_timestamp(ts: &str) -> String {
    // Timestamps come as "2026-01-16T03:43:21.123456" - extract date and time
    if let Some(t_idx) = ts.find('T') {
        let date = &ts[..t_idx];
        let time = &ts[t_idx + 1..];
        // Take only HH:MM:SS from time
        let time_short = time.split('.').next().unwrap_or(time);
        format!("{} {}", date, time_short)
    } else {
        ts.to_string()
    }
}

pub fn get_str<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("-")
}

pub fn get_i64(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

pub fn get_bool(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

pub fn print_json(value: &Value) {
    println!("{}", serde_json::to_string_pretty(value).unwrap());
}

pub fn handle_response(result: Result<Value, String>, raw: bool) {
    handle_response_with(result, raw, print_json)
}

pub fn handle_response_with<F>(result: Result<Value, String>, raw: bool, format_fn: F)
where
    F: FnOnce(&Value),
{
    match result {
        Ok(json) => {
            let status = json.get("status").and_then(|s| s.as_i64()).unwrap_or(0);
            if status == 200 {
                if raw {
                    print_json(&json);
                } else {
                    format_fn(&json);
                }
            } else {
                let error = json.get("error").and_then(|e| e.as_str()).unwrap_or("Unknown error");
                eprintln!("Error: {}", error);
                std::process::exit(1);
            }
        }
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

pub fn format_me(json: &Value) {
    if let Some(user_wrapper) = json.get("user") {
        let user = user_wrapper.get("user").unwrap_or(user_wrapper);
        let ntfy = user_wrapper.get("ntfy");

        println!("User Info");
        println!("=========");
        println!("ID:           {}", get_str(user, "id"));
        println!("Type:         {}", get_str(user, "user_type"));
        println!("Language:     {}", get_str(user, "language_code"));
        println!("Created:      {}", format_timestamp(get_str(user, "created_at")));
        println!("Up delay:     {}s", get_i64(user, "up_delay"));
        let mw_start = user.get("maint_window_start_utc").and_then(|v| v.as_i64());
        let mw_end = user.get("maint_window_end_utc").and_then(|v| v.as_i64());
        match (mw_start, mw_end) {
            (Some(s), Some(e)) => println!("Maintenance:  {:02}:{:02}-{:02}:{:02} UTC", s / 60, s % 60, e / 60, e % 60),
            _ => println!("Maintenance:  [not set]"),
        }
        println!("Invites:      {}/{}", get_i64(user, "invites_used"), get_i64(user, "invites_limit"));

        if let Some(ntfy) = ntfy {
            println!();
            println!("Ntfy.sh       {}", bool_icon(get_bool(ntfy, "enabled")));
            println!("  Topic:      {}", get_str(ntfy, "topic"));
            println!("  Username:   {}", get_str(ntfy, "username"));
        }
    } else {
        print_json(json);
    }
}

pub fn format_users_list(json: &Value) {
    if let Some(users) = json.get("users").and_then(|u| u.as_array()) {
        if users.is_empty() {
            println!("No users found.");
            return;
        }
        println!("{:<8} {:<36} {:>7} {:>10}", "TYPE", "ID", "NTFY", "INVITES");
        println!("{}", "-".repeat(65));
        for user_wrapper in users {
            let user = user_wrapper.get("user").unwrap_or(user_wrapper);
            let ntfy = user_wrapper.get("ntfy");

            let user_type = get_str(user, "user_type");
            let id = get_str(user, "id");
            let ntfy_on = ntfy.map(|n| get_bool(n, "enabled")).unwrap_or(false);
            let invites = format!("{}/{}", get_i64(user, "invites_used"), get_i64(user, "invites_limit"));

            println!("{:<8} {:<36} {:>7} {:>10}", user_type, id, bool_icon(ntfy_on), invites);
        }
        println!();
        println!("Total: {} user(s)", users.len());
    } else {
        print_json(json);
    }
}

pub fn format_invites_list(json: &Value) {
    if let Some(invites) = json.get("invites").and_then(|i| i.as_array()) {
        if invites.is_empty() {
            println!("No invites found.");
            return;
        }
        println!("{:<36} {:<20}", "ID", "CREATED");
        println!("{}", "-".repeat(58));
        for invite in invites {
            let id = get_str(invite, "id");
            let created = format_timestamp(get_str(invite, "created_at"));

            println!("{:<36} {:<20}", id, created);
        }
        println!();
        println!("Total: {} invite(s)", invites.len());
    } else {
        print_json(json);
    }
}

pub fn format_ntfy(json: &Value) {
    if let Some(ntfy) = json.get("ntfy") {
        println!("Ntfy.sh Settings");
        println!("================");
        println!("Enabled:      {}", bool_icon(get_bool(ntfy, "enabled")));
        println!("Topic:        {}", get_str(ntfy, "topic"));
        println!("Username:     {}", get_str(ntfy, "username"));
        println!("Password:     {}", get_str(ntfy, "password"));
    } else {
        print_json(json);
    }
}

pub fn format_settings_update(json: &Value) {
    println!("Settings updated:");
    if let Some(v) = json.get("up_delay").and_then(|v| v.as_i64()) {
        println!("  up_delay:     {}s", v);
    }
    let start = json.get("maint_window_start_utc");
    let end = json.get("maint_window_end_utc");
    if start.is_some() || end.is_some() {
        match (start.and_then(|v| v.as_i64()), end.and_then(|v| v.as_i64())) {
            (Some(s), Some(e)) => println!("  maintenance:  {:02}:{:02}-{:02}:{:02} UTC", s / 60, s % 60, e / 60, e % 60),
            _ => println!("  maintenance:  [cleared]"),
        }
    }
}
