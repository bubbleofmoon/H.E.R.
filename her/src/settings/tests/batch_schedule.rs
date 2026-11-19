use super::*;

fn schedule_setting(context: &RuntimeContext, key: &str, value: Value) -> (u16, Value) {
    post(context, "/api/settings/set", json!({"setting":key,"value":value}))
}

#[test]
fn schedule_collision_checks_cover_set_reset_and_undo() {
    let (root, context) = settings_fixture("schedule settings");
    let (status, body) = schedule_setting(&context, "memory.digest.time", json!("03:31"));
    assert_eq!(status, 409, "a first set at 29 minutes must refuse: {body}");
    assert!(!settings_db(&root).exists());
    assert_eq!(schedule_setting(&context, "memory.digest.time", json!("05:00")).0, 200);
    let (history, _) = history_page(&context, "?setting=memory.digest.time");
    let entry = history[0]["entry"].clone();
    assert_eq!(schedule_setting(&context, "backup.time", json!("03:15")).0, 200);
    let before = audit_rows(&root);
    assert_eq!(post(&context, "/api/settings/reset", json!({"setting":"memory.digest.time"})).0, 409);
    assert_eq!(post(&context, "/api/settings/undo", json!({"entry":entry})).0, 409);
    assert_eq!(audit_rows(&root), before);
    assert_eq!(context.load_config().unwrap()["memory"]["digest"]["time"], "05:00");
    assert_eq!(schedule_setting(&context, "backup.time", json!("03:30")).0, 200);
    assert_eq!(post(&context, "/api/settings/undo", json!({"entry":entry})).0, 200);
    assert_eq!(context.load_config().unwrap()["memory"]["digest"]["time"], "03:00");
}

#[test]
fn backup_page_uses_store_and_refuses_collisions_across_midnight() {
    let (root, context) = settings_fixture("backup schedule settings");
    assert_eq!(schedule_setting(&context, "memory.digest.time", json!("23:45")).0, 200);
    let file = stored_config(&root);
    let endpoint = "/api/backups/settings";
    let (status, reply) = post(&context, endpoint, json!({"time":"00:14","keepDays":12}));
    assert_eq!(status, 409, "a backup page collision must refuse both values: {reply}");
    assert_eq!(stored_config(&root), file);
    let (status, reply) = post(&context, endpoint, json!({"time":"00:15","keepDays":12}));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["settings"]["time"], "00:15");
    assert_eq!(stored_config(&root)["backup"]["keepDays"], 12);
    assert_eq!(setting_view(&context, "backup.time")["stored"], "00:15");
    let (history, _) = history_page(&context, "?setting=backup.time");
    assert_eq!(history[0]["source"], "console");
}

#[test]
fn digest_schedule_refresh_applies_live_times_without_eating_due_slots() {
    use crate::tasks::{Schedule, Scheduler, TaskId};
    let (_root, context) = settings_fixture("digest schedule refresh");
    let mut scheduler = Scheduler::new(|def| matches!(def.id, TaskId::MemoryDigest | TaskId::MemoryDigestCheck), 0, 0);
    let (status, body) = schedule_setting(&context, "memory.digest.time", json!("06:00"));
    assert_eq!(status, 200, "{body}");
    assert_eq!(schedule_setting(&context, "memory.digest.checkMinute", json!(0)).0, 409);
    assert_eq!(schedule_setting(&context, "memory.digest.checkMinute", json!(2)).0, 200);
    let cfg = context.load_config().unwrap();
    scheduler.refresh_schedules(&cfg, 0);
    assert_eq!(scheduler.schedule(TaskId::MemoryDigest), Some(Schedule::Daily { hour: 6, minute: 0 }), "live digest time");
    assert_eq!(scheduler.schedule(TaskId::MemoryDigestCheck), Some(Schedule::Every { minutes: 30, offset: 2 }));
    let due = 6 * 3_600_000;
    scheduler.refresh_schedules(&cfg, due);
    assert!(scheduler.tick(due).iter().any(|run| run.task == TaskId::MemoryDigest && run.slot == due));
    assert!(scheduler.tick(due).is_empty());
}
