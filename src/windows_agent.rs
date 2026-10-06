//! Named operations use the scene's own input path; no OS cursor or keyboard injection.
use super::*;

const HELP: &str = "pleamar-wm agent — native Windows scene commands

  scenes                         running pleamar scenes, their PIDs and command endpoints (JSON)
  windows                        ordinary native windows, with a scene name where known (JSON)
  monitors                       native monitor catalog (JSON)
  tree PID [json]                visible elements, labels, states and logical geometry
  press PID NAME [right|middle] [COUNT]
                                 press a named scene element and report what changed
  wait PID CONDITION [TIMEOUT]    wait for a scene condition, for example saved == true 3s
  watch PID [SECONDS]            stream events and changes as they happen
  say PID ORDER...               another scene order: type query words, drag knob 0 -40,
                                 hold card, wheel list -3, key escape

PID comes from scenes or windows. PID.N also addresses that process's scene.
If one process has several endpoints, use scene:ENDPOINT to select one.
Panels such as Marea may appear only in scenes, not the ordinary window catalog.
This does not provide Linux's independent pointer, keyboard seat or cursor glide.
Arbitrary applications, look/click/open/send and remote control remain unavailable.
All commands use the current logon's scene namespace.";

#[derive(Clone, Debug, Serialize, PartialEq)]
struct Scene { pid: u32, scene: String, endpoint: String }

fn hello(endpoint: &str, answer: &str) -> Option<Scene> {
    if !answer.starts_with("pleamar ") { return None; }
    let field = |key: &str| answer.split(" · ").find_map(|s| s.strip_prefix(key));
    let pid = field("pid ")?.trim().parse::<u32>().ok().filter(|pid| *pid > 0)?;
    let scene = field("scene ")?.trim();
    if scene.is_empty() { return None; }
    Some(Scene { pid, scene: scene.to_owned(), endpoint: endpoint.to_owned() })
}

fn scenes() -> Result<Vec<Scene>> {
    let mut found = Vec::new();
    let until = Instant::now() + Duration::from_secs(4);
    for endpoint in pleamar::commands::running_scenes() {
        if Instant::now() >= until { return Err("scene discovery timed out; close unresponsive command endpoints and retry".into()); }
        if let Ok(answer) = pleamar::commands::ask(&endpoint, "hello", Duration::from_millis(250)) {
            if let Some(scene) = hello(&endpoint, &answer) { found.push(scene); }
        }
    }
    Ok(found)
}

fn select<'a>(scenes: &'a [Scene], selector: &str) -> Result<&'a Scene> {
    if let Some(endpoint) = selector.strip_prefix("scene:") {
        return scenes.iter().find(|s| s.endpoint == endpoint).ok_or_else(|| "that scene endpoint did not answer hello".into());
    }
    let mut parts = selector.split('.');
    let pid = parts.next().and_then(|n| n.parse::<u32>().ok()).filter(|n| *n > 0).ok_or("use a PID from agent scenes")?;
    if let Some(index) = parts.next() {
        if index.parse::<u32>().is_err() || parts.next().is_some() { return Err("use PID or PID.N from agent windows".into()); }
    }
    let mut matches = scenes.iter().filter(|s| s.pid == pid);
    let first = matches.next().ok_or("this process has no responding pleamar scene; arbitrary application input is not implemented on Windows")?;
    if matches.next().is_some() { return Err("this process has several scenes; use scene:ENDPOINT from agent scenes".into()); }
    Ok(first)
}

pub(super) fn execute(args: &[&str]) -> Result<Option<Value>> {
    match args {
        [] | ["help" | "--help" | "-h"] => { println!("{HELP}"); Ok(None) },
        ["scenes"] => Ok(Some(serde_json::to_value(scenes()?)?)),
        ["monitors"] => Ok(Some(serde_json::to_value(monitors()?)?)),
        ["windows"] => {
            let scenes = scenes()?;
            let list = windows()?.into_iter().map(|w| {
                let matching: Vec<_> = scenes.iter().filter(|s| s.pid == w.process).collect();
                json!({"window":w,"scenes":matching})
            }).collect::<Vec<_>>();
            Ok(Some(json!(list)))
        },
        [what @ ("tree" | "press" | "wait" | "watch" | "say"), selector, rest @ ..] => {
            let line = match (*what, rest) {
                ("tree", []) => "describe".to_owned(),
                ("tree", ["json"]) => "describe json".to_owned(),
                ("tree", _) => return Err("tree PID [json]".into()),
                ("watch", [] | [_]) => format!("watch {}", rest.join(" ")),
                ("press" | "wait" | "say", []) => return Err(format!("{what} needs a scene command or element").into()),
                ("say", _) => rest.join(" "),
                ("press" | "wait", _) => format!("{what} {}", rest.join(" ")),
                _ => return Err("watch PID [SECONDS]".into()),
            };
            let scenes = scenes()?;
            let scene = select(&scenes, selector)?;
            pleamar::commands::send(Some(&scene.endpoint), line.trim()).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            Ok(None)
        },
        _ => Err("unsupported Windows agent operation; use agent help for native scene commands and remaining limitations".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hello_keeps_unicode_names_and_rejects_other_protocols() {
        let answer = "pleamar 0.2.25 · scene Mi escena ñ · pid 123 · language 0.2";
        assert_eq!(hello("Mi escena ñ-123", answer), Some(Scene {pid:123,scene:"Mi escena ñ".into(),endpoint:"Mi escena ñ-123".into()}));
        for answer in ["? unknown hello", "pleamar x · scene x · pid 0", "other x · scene x · pid 123", "pleamar x · pid 123"] {
            assert_eq!(hello("x", answer), None);
        }
    }
    #[test]
    fn process_selection_refuses_ambiguity_and_malformed_numbers() {
        let mut scenes = vec![Scene {pid:123,scene:"Marea".into(),endpoint:"Marea".into()}];
        assert_eq!(select(&scenes,"123.2").unwrap().endpoint,"Marea");
        for selector in ["0","123.","123.2.3","-123","456"] { assert!(select(&scenes,selector).is_err()); }
        scenes.push(Scene {pid:123,scene:"Second".into(),endpoint:"Second".into()});
        assert!(select(&scenes,"123").is_err());
        assert_eq!(select(&scenes,"scene:Marea").unwrap().scene,"Marea");
    }
}
