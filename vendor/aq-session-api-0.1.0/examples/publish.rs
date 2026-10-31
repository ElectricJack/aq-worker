fn main() {
    println!(
        "{}",
        serde_json::to_string_pretty(&aq_session_api::schema()).unwrap()
    );
}
