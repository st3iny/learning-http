use anyhow::Result;

mod http;

fn main() -> Result<()> {
    let client = http::Client::new(http::HttpVersion::Http1_1);
    let response = client.get("https://google.com/")?;

    println!(
        "\n\n{} {}",
        response.status_code(),
        response.status_message()
    );
    for (key, value) in response.headers().iter() {
        println!("{key}: {value}")
    }
    println!();
    println!("{}", String::from_utf8_lossy(response.body()));

    println!("\n\nDone!");
    Ok(())
}
