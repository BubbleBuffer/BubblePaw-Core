use super::workspace_edit::Fixture;
use serde_json::{Value, json};
use std::fs;

#[tokio::test]
async fn edit_ignores_line_endings_and_bom_and_applies_several_edits_all_or_nothing() {
    let f = Fixture::new();
    let two = |old: &str, new: &str| json!({"old_string":old,"new_string":new});
    let cases: [(&[u8], Value, &[u8]); 5] = [
        (
            b"a\r\nold\r\nb\r\n",
            two("a\nold\n", "a\nx\ny\n"),
            b"a\r\nx\r\ny\r\nb\r\n",
        ),
        (
            b"a\nold\nb\n",
            two("a\r\nold\r\n", "a\r\nx\r\n"),
            b"a\nx\nb\n",
        ),
        (
            b"\xEF\xBB\xBFa\r\nold\r\n",
            two("a\nold", "a\nnew"),
            b"\xEF\xBB\xBFa\r\nnew\r\n",
        ),
        (
            b"a\r\nb\r\nc\nold\r\n",
            two("old", "x\ny"),
            b"a\r\nb\r\nc\nx\r\ny\r\n",
        ),
        (
            b"one\r\ntwo\r\n",
            json!({"edits":[two("one", "two"), two("two\ntwo", "x")]}),
            b"x\r\n",
        ),
    ];
    for (original, mut input, expected) in cases {
        let path = f.root.path().join("a.txt");
        fs::write(&path, original).unwrap();
        input["path"] = "a.txt".into();
        f.call("edit", input).await.unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected);
    }
    let path = f.root.path().join("a.txt");
    fs::write(&path, b"one\r\ntwo\r\n").unwrap();
    let edits = json!({"path":"a.txt","edits":[two("one", "1"), two("missing", "z")]});
    let error = f.call("edit", edits).await.unwrap_err().to_string();
    assert!(
        error.contains("edits[1]") && error.contains("no exact match"),
        "{error}"
    );
    let both =
        json!({"path":"a.txt","old_string":"one","new_string":"1","edits":[two("two", "2")]});
    assert!(f.call("edit", both).await.is_err());
    assert_eq!(fs::read(&path).unwrap(), b"one\r\ntwo\r\n");
}
