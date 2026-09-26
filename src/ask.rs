//! Всплывающее окно VibeTerminal: когда ты в другом приложении, а Claude
//! просит разрешение, задаёт вопрос или предлагает план, — ответить можно
//! прямо в окне поверх всех программ, не переключаясь в терминал.
//!
//! vv говорит VibeTerminal «покажи окно» (OSC 777 с меткой `⟦vv-ask:…⟧`),
//! приложение по сокету vv забирает описание запроса (`describe`) и
//! присылает ответ, из которого vv собирает решение для Claude (`decision`).

use std::path::Path;

use serde_json::{Value, json};

use crate::hooks::{self, PermissionRequest};

/// Что показать в окне.
pub fn describe(request: &PermissionRequest, session: &str, cwd: &Path, home: &Path) -> Value {
    let place = cwd.strip_prefix(home).map_or_else(|_| cwd.display().to_string(), |p| format!("~/{}", p.display()));
    let base = json!({ "session": session, "place": place });
    let mut out = match request.tool.as_str() {
        "AskUserQuestion" => json!({
            "kind": "question",
            "title": "Claude спрашивает",
            "questions": request.input["questions"].as_array().cloned().unwrap_or_default().iter().map(|q| json!({
                "question": q["question"],
                "header": q["header"],
                "multi": q["multiSelect"].as_bool().unwrap_or(false),
                "options": q["options"].as_array().cloned().unwrap_or_default().iter().map(|o| json!({
                    "label": o["label"],
                    "description": o["description"],
                })).collect::<Vec<_>>(),
            })).collect::<Vec<_>>(),
        }),
        "ExitPlanMode" => json!({
            "kind": "plan",
            "title": "Claude предлагает план",
            "plan": request.input["plan"].as_str().unwrap_or_default(),
        }),
        tool => {
            let (what, detail) = hooks::describe(tool, &request.input, cwd);
            json!({
                "kind": "permission",
                "title": format!("{what}?"),
                "detail": detail,
                "note": request.input["description"].as_str().unwrap_or_default(),
                "always": always_label(&request.suggestions),
            })
        }
    };
    if let (Some(out), Some(base)) = (out.as_object_mut(), base.as_object()) {
        out.extend(base.clone());
    }
    out
}

/// Подпись для «разрешать всегда» из того, что предлагает сам Claude.
/// Нечего предложить — `null`, кнопки не будет.
fn always_label(suggestions: &Value) -> Value {
    let Some(first) = suggestions.as_array().and_then(|s| s.first()) else { return Value::Null };
    match first["type"].as_str() {
        Some("addRules") => {
            let rules: Vec<String> = first["rules"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|rule| match rule["ruleContent"].as_str() {
                    Some(content) if !content.is_empty() => content.to_string(),
                    _ => rule["toolName"].as_str().unwrap_or_default().to_string(),
                })
                .collect();
            let place = match first["destination"].as_str() {
                Some("userSettings") => "везде",
                Some("session") => "до конца сессии",
                _ => "в этом проекте",
            };
            json!(format!("Разрешать всегда {place}: {}", rules.join(", ")))
        }
        Some("setMode") if first["mode"] == "acceptEdits" => json!("Разрешать правку файлов до конца сессии"),
        Some("addDirectories") => json!("Разрешать эту папку до конца сессии"),
        _ => json!("Разрешать так всегда"),
    }
}

/// Ответ из окна → решение для хука Claude. `None` — ответ непонятный.
pub fn decision(request: &PermissionRequest, answer: &Value) -> Option<Value> {
    let decision = match (request.tool.as_str(), answer["choice"].as_str()) {
        ("AskUserQuestion", _) => {
            let answers = answer["answers"].as_object()?;
            let mut input = request.input.clone();
            input.as_object_mut()?.insert("answers".into(), Value::Object(answers.clone()));
            json!({ "behavior": "allow", "updatedInput": input })
        }
        ("ExitPlanMode", Some("approve")) => json!({ "behavior": "allow", "updatedInput": request.input }),
        ("ExitPlanMode", Some("revise")) => {
            let feedback = answer["feedback"].as_str().map(str::trim).filter(|f| !f.is_empty());
            let message = feedback.map_or_else(
                || "Пользователь не утвердил план. Доработай его.".to_string(),
                |f| format!("Пользователь не утвердил план. Что поправить: {f}"),
            );
            json!({ "behavior": "deny", "message": message })
        }
        (_, Some("allow")) => json!({ "behavior": "allow" }),
        (_, Some("always")) => json!({ "behavior": "allow", "updatedPermissions": request.suggestions }),
        (_, Some("deny")) => json!({ "behavior": "deny", "message": hooks::DENY_MESSAGE }),
        _ => return None,
    };
    Some(decision)
}

/// Метка в заголовке OSC 777: VibeTerminal вместо баннера покажет окно.
pub fn marker(session: u64, request: u64, socket: &Path) -> String {
    format!("⟦vv-ask:{session}:{request}:{}⟧", socket.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::PermissionRequest;

    fn request(tool: &str, input: Value, suggestions: Value) -> PermissionRequest {
        PermissionRequest::for_test(tool, input, suggestions)
    }

    #[test]
    fn describes_permission_with_always_option() {
        let suggestions = json!([{ "type": "addRules", "rules": [{ "toolName": "Bash", "ruleContent": "npm --version" }], "behavior": "allow", "destination": "localSettings" }]);
        let req = request("Bash", json!({ "command": "npm --version", "description": "Версия npm" }), suggestions);
        let out = describe(&req, "api", Path::new("/Users/me/Projects/api"), Path::new("/Users/me"));
        assert_eq!(out["kind"], "permission");
        assert_eq!(out["title"], "Выполнить команду?");
        assert_eq!(out["detail"], "$ npm --version");
        assert_eq!(out["note"], "Версия npm");
        assert_eq!(out["always"], "Разрешать всегда в этом проекте: npm --version");
        assert_eq!(out["place"], "~/Projects/api");
        let edits = json!([{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }]);
        assert_eq!(always_label(&edits), "Разрешать правку файлов до конца сессии");
        assert_eq!(always_label(&Value::Null), Value::Null);
    }

    #[test]
    fn builds_decisions_for_every_kind() {
        let bash = request("Bash", json!({ "command": "ls" }), json!([{ "type": "setMode", "mode": "acceptEdits" }]));
        assert_eq!(decision(&bash, &json!({ "choice": "allow" })).unwrap(), json!({ "behavior": "allow" }));
        assert_eq!(decision(&bash, &json!({ "choice": "always" })).unwrap()["updatedPermissions"][0]["mode"], "acceptEdits");
        assert_eq!(decision(&bash, &json!({ "choice": "deny" })).unwrap()["behavior"], "deny");
        assert!(decision(&bash, &json!({ "choice": "что-то" })).is_none());

        let question = request("AskUserQuestion", json!({ "questions": [{ "question": "Цвет?" }] }), Value::Null);
        let answered = decision(&question, &json!({ "answers": { "Цвет?": "Синий" } })).unwrap();
        assert_eq!(answered["updatedInput"]["answers"]["Цвет?"], "Синий");
        assert_eq!(answered["updatedInput"]["questions"][0]["question"], "Цвет?");

        let plan = request("ExitPlanMode", json!({ "plan": "1. Сделать" }), Value::Null);
        assert_eq!(decision(&plan, &json!({ "choice": "approve" })).unwrap()["updatedInput"]["plan"], "1. Сделать");
        let revise = decision(&plan, &json!({ "choice": "revise", "feedback": "без тестов" })).unwrap();
        assert_eq!(revise["behavior"], "deny");
        assert!(revise["message"].as_str().unwrap().contains("без тестов"));
    }
}
