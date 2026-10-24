use super::*;

#[tokio::test]
async fn syntax_and_runtime_errors_report_the_original_cell_line_and_column() {
    let f = Fixture::new();
    let syntax = f.cell_error("const a=1;\nconst b=2;\nconst c = ;").await;
    assert!(syntax.contains("line 3, column "), "{syntax}");
    let runtime = f
        .cell_error("const a=1;\nconst b=2;\nthrow Error('third line');")
        .await;
    assert!(runtime.contains("third line (line 3, column "), "{runtime}");
    let expression = f
        .cell_error("(\n  ()=>{throw Error('expression');}\n)()")
        .await;
    assert!(
        expression.contains("expression (line 2, column "),
        "{expression}"
    );
    f.manager.shutdown().await;
}
