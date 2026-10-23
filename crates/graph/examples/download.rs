//! Manual check of resumable downloads:
//! `cargo run -p odrs-graph --example download -- <refresh_token file> <drive id> <item id> <dest>`
//! An existing `<dest>` is resumed from its current length.

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [token, drive, item, dest] = args.as_slice() else {
        return Err("usage: download <refresh_token file> <drive id> <item id> <dest>".into());
    };
    let graph = odrs_graph::Graph::new(odrs_graph::http_client()?, Default::default(), odrs_graph::TokenStore::new(token))?;
    let (hash, size) = graph.download_to(drive, item, std::path::Path::new(dest)).await?;
    println!("{size} bytes, quickXorHash {hash}");
    Ok(())
}
