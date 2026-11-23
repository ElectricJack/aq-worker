use super::*;

/// Published structural schema. Cross-field, digest, byte-size and authority rules
/// are enforced by Validate and are normative alongside this schema.
pub fn contract_schema() -> serde_json::Value {
    let mut s =
        serde_json::to_value(schemars::schema_for!(ContractDocument)).expect("schema is JSON");
    let defs = s["$defs"].as_object_mut().expect("definitions");
    for (name, field, max) in [
        ("LaunchManifest", "modules", 64),
        ("LaunchManifest", "credentials", 16),
        ("HarnessSettings", "required_capabilities", 64),
        ("ProfiledOffer", "required_capabilities", 64),
        ("ProfiledAcceptance", "advertised_capabilities", 64),
        ("PermissionPlan", "requested", 128),
        ("PermissionPlan", "effective", 128),
        ("PermissionPlan", "required", 128),
        ("PermissionPlan", "omissions", 128),
        ("ContextPlan", "references", 128),
        ("InputCommand", "bytes", 4096),
        ("OutputFrame", "bytes", 65536),
        ("Snapshot", "state", 196608),
        ("NodeCommandBatch", "commands", 64),
        ("NodeOutputBatch", "frames", 64),
        ("NodeOutputBatch", "snapshots", 32),
        ("NodeOutputBatch", "gaps", 64),
        ("NodeOutputBatch", "receipts", 64),
    ] {
        defs.get_mut(name).expect("known type")["properties"][field]["maxItems"] = max.into();
    }
    for name in [
        "LaunchManifest",
        "LaunchBinding",
        "NodeTerminalPoll",
        "NodeCommandBatch",
        "NodeOutputBatch",
    ] {
        defs.get_mut(name).expect("known type")["properties"]["schema_version"]["const"] = 1.into();
    }
    for (name, field, max) in [
        ("TerminalPolicy", "max_input_bytes", 4096),
        ("TerminalPolicy", "max_pending_input_bytes", 32768),
        ("TerminalPolicy", "max_frame_bytes", 65536),
        ("TerminalPolicy", "replay_bytes", 1048576),
        ("ContextPlan", "startup_byte_budget", 1048576),
        ("ContextRef", "bytes", 1048576),
        ("ContextRef", "tokens", 1048576),
        ("NodeTerminalPoll", "hold_ms", 20000),
        ("NodeTerminalPoll", "active_subscriptions", 32),
        ("ResizeCommand", "columns", 1000),
        ("ResizeCommand", "rows", 1000),
        ("Snapshot", "columns", 1000),
        ("Snapshot", "rows", 1000),
    ] {
        defs.get_mut(name).expect("known type")["properties"][field]["maximum"] = max.into();
    }
    s
}
