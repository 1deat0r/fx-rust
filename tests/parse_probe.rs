#[test]
fn probe_exact_model_text() {
    let text = "I'll look up trending AI news for today.\n\n<tool_calls>\n<invoke name=\"web_search\">\n<parameter name=\"query\">trending AI news today</parameter>\n<parameter name=\"num_results\" string=\"false\">10</parameter>\n</invoke>\n</tool_calls>";
    let calls = fxrs::tools::parse_text_tool_calls(text);
    eprintln!("CALLS: {:?}", calls);
    assert_eq!(calls.len(), 1, "expected 1 call, got {calls:?}");
}
