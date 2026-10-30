//! A stand-in program for `pult tool`'s tests (tests/tool.rs): it describes
//! one command in a `self schema` document, and otherwise prints the arguments
//! it received as a JSON array — exactly what the OS handed it, so a test can
//! compare argv byte for byte on every platform, Windows' quoting included.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args == ["self", "schema", "--json"] {
        print!("{DOCUMENT}");
        return;
    }
    println!(
        "{}",
        serde_json::to_string(&args).expect("strings serialize")
    );
}

const DOCUMENT: &str = r#"{"lonic_json":1,"tool":"argv-echo","schema":1,"command":"self","ok":true,"data":{
  "version": 1,
  "name": "argv-echo",
  "commands": [
    {
      "id": "echo",
      "title": "Print the arguments received",
      "params": {
        "loud": {"flag": {"arg": "--loud"}},
        "grep": {"repeat": {"arg": "--grep", "max": 1}},
        "items": {"repeat": {}}
      },
      "run": "echo {loud} {grep} -- {items}",
      "interactive": false
    }
  ]
}}
"#;
