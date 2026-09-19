use std::error::Error;

#[tokio::main]
async fn main() {
    let url = "https://open.bigmodel.cn/api/coding/paas/v4/chat/completions";
    let client = reqwest::Client::new();
    match client
        .post(url)
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
    {
        Ok(r) => println!("OK status={}", r.status()),
        Err(e) => {
            println!("ERR: {e}");
            let mut src: Option<&dyn Error> = Some(&e);
            while let Some(s) = src {
                println!("  caused by: {s}");
                src = s.source();
            }
        }
    }
}
