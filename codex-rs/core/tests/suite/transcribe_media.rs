#![cfg(not(target_os = "windows"))]

use anyhow::Result;
use core_test_support::responses::mount_function_call_agent_response;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transcribe_media_tool_returns_transcript_text() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let test = test_codex().build(&server).await?;

    let media_path = test.cwd.path().join("media/sample.mp4");
    std::fs::create_dir_all(
        media_path
            .parent()
            .expect("sample media path should have parent"),
    )?;
    std::fs::write(&media_path, b"fake mp4 bytes for test transcript")?;

    let transcript = "hello from a mocked transcript";
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(header("authorization", "Bearer dummy"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "text": transcript,
        })))
        .expect(1)
        .mount(&server)
        .await;

    let call_id = "transcribe-media-call";
    let arguments = json!({
        "path": "media/sample.mp4",
    })
    .to_string();

    let mocks =
        mount_function_call_agent_response(&server, call_id, &arguments, "transcribe_media").await;

    test.submit_turn("please transcribe this media file")
        .await?;

    let req = mocks.completion.single_request();
    let (content_opt, success_opt) = req
        .function_call_output_content_and_success(call_id)
        .expect("function_call_output should be present");
    let content = content_opt.expect("function_call_output content should be present");
    if let Some(success) = success_opt {
        assert!(success, "transcribe_media should return success=true");
    }
    assert_eq!(content, transcript);

    server.verify().await;
    Ok(())
}
