fn main() {
    match z13helper_client::Client::new().get_state() {
        Ok(state) => println!("{}", serde_json::to_string_pretty(&state).unwrap()),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
