//! `hidden_character` and `visible_body` agree with the fixture Harness Desktop checks its own
//! copy of the rule against (`ui/src/features/memory/hidden-chars.fixture.json`).

use harness_memory::store::{hidden_character, visible_body};
use pretty_assertions::assert_eq;
use serde_json::Value;

#[test]
fn the_hidden_character_rule_matches_the_shared_fixture() {
    let fixture: Value = serde_json::from_str(include_str!("fixtures/hidden-chars.json")).expect("fixture");
    let cases = fixture["cases"].as_array().expect("cases");

    assert!(!cases.is_empty());

    for case in cases {
        let name = case["name"].as_str().expect("name");
        let text = case["text"].as_str().expect("text");
        let hidden: Vec<usize> = case["hidden"]
            .as_array()
            .expect("hidden")
            .iter()
            .map(|index| usize::try_from(index.as_u64().expect("index")).expect("index"))
            .collect();
        let characters: Vec<char> = text.chars().collect();

        let first = hidden.first().map(|&index| {
            let line = 1 + characters[..index]
                .iter()
                .filter(|&&character| character == '\n')
                .count();
            (characters[index], line)
        });
        assert_eq!(hidden_character(text), first, "first hidden character of: {name}");

        let visible: String = characters
            .iter()
            .enumerate()
            .filter(|&(index, &character)| {
                !hidden.contains(&index) && !(character == '\r' && characters.get(index + 1) == Some(&'\n'))
            })
            .map(|(_, &character)| character)
            .collect();
        assert_eq!(visible_body(text), visible, "visible body of: {name}");
    }
}
