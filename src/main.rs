use std::collections::HashMap;
use std::error::Error;
use std::time::{Duration, Instant};

use bstr::ByteSlice;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use rand::Rng;
use reqwest::Client;
use reqwest::redirect::Policy;
use tokio::time::sleep;

/// Number of parallel runtime(s)
static mut PARALLEL: usize = 1;
/// Limit number of request per sec per PARALLEL
static mut REQ_PER_SEC: usize = 128;
/// request count per PARALLEL (exit after reached 0 for unlimited)
static mut COUNTS: usize = 1_000_000;
/// use http2 only
static mut USE_H2: bool = false;
/// url to fake page
static mut HOST: &str = "";
/// url for referer (will skip parse_site and use HOST as target)
static mut DIRECT_HOST: &str = "";

/// Change this function to edit body on each request
fn transform_body(mut h: HashMap<&'static str, String>) -> HashMap<&'static str, String> {
	h.insert("accountName", rand_text(128));
	h.insert("password", rand_text(64));
	if !h.contains_key("domain") {
		h.insert("domain", unsafe { HOST }.to_string());
	}
	h
}

/// Change this function to edit how to parse the site
async fn parse_site(url: &'static str) -> Result<Option<Site>, Box<dyn Error>> {
	let resp = reqwest::get(url).await?;
	if !resp.status().is_success() {
		return Ok(None);
	}
	let bytes = resp.bytes().await?;
	let mut site: &[u8] = &[];
	for x in bytes.as_bstr().lines() {
		if x.trim().starts_with(b"window.$loginLink") {
			let left = x.find(b"\"");
			let right = x.rfind(b"\"");
			if let (Some(left), Some(right)) = (left, right) {
				if left == right {
					return Ok(None);
				}
				site = &x[left + 1..right];
			}
		}
	}
	Ok(Some(Site { url: site.as_bstr().to_string(), referer: url }))
}

fn main() {
	init();
}

const CHARS: [char; 36] = ['0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z'];

fn rand_text(len: usize) -> String {
	let mut str = String::with_capacity(len);
	let mut rng = rand::thread_rng();
	for _ in 0..len {
		str.push(CHARS[rng.gen_range(0..CHARS.len())]);
	}
	str
}

fn new_client() -> Client {
	let mut builder = Client::builder()
		.redirect(Policy::none());
	if unsafe { USE_H2 } {
		builder = builder.http2_prior_knowledge().use_rustls_tls()
	}
	return builder.build().unwrap();
}

fn init() {
	let parallel = unsafe { PARALLEL };
	let runtime = tokio::runtime::Builder::new_current_thread()
		.enable_all()
		.global_queue_interval(31)
		.build()
		.unwrap();

	let runtimes = runtime.block_on(async move {
		let site = parse_site(unsafe { HOST }).await.unwrap();
		if site.is_none() {
			eprintln!("Can't parse from this site!");
			return Vec::new();
		}
		let site = site.unwrap();
		println!("Using config: {:#?}", site);
		let url: &'static str = {
			if !unsafe { DIRECT_HOST }.is_empty() {
				unsafe { HOST }
			} else {
				Box::leak(site.url.into_boxed_str())
			}
		};
		let referer = {
			if !unsafe { DIRECT_HOST }.is_empty() {
				unsafe { DIRECT_HOST }
			} else {
				site.referer
			}
		};
		let fut = FuturesUnordered::new();
		let mut runtimes = Vec::with_capacity(parallel);
		for _ in 0..parallel {
			let rt = tokio::runtime::Builder::new_multi_thread()
				.enable_all()
				.global_queue_interval(31)
				.worker_threads(2)
				.build()
				.unwrap();
			let f = say_hello(new_client(), url, referer, unsafe { COUNTS }, transform_body);
			let f = rt.spawn(f);
			fut.push(f);
			runtimes.push(rt);
		}
		let _: Vec<_> = fut.collect().await;
		runtimes
	});

	for r in runtimes {
		r.shutdown_background();
	}
	runtime.shutdown_background();
}

async fn say_hello<F>(client: Client, url: &'static str, referer: &'static str, limit: usize, transform_body: F)
	where F: Fn(HashMap<&'static str, String>) -> HashMap<&'static str, String> {
	let mut i: usize = 0;
	let chunk_size = (1 << unsafe { REQ_PER_SEC as f32 }.log2().ceil() as usize) - 1;
	println!("using chunk size = {:?}", chunk_size);

	let mut futures = FuturesUnordered::new();
	let rate = unsafe { REQ_PER_SEC };
	let mut last = Instant::now();
	let mut body = transform_body(HashMap::new());
	let prep = client.post(url)
		.header("Sec-Ch-Ua", r#""Chromium";v="105", "Not)A;Brand";v="8""#)
		.header("User-Agent", r#"Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/105.0.5195.127 Safari/537.36"#)
		.header("Origin", referer)
		.header("Referer", referer);
	loop {
		let f = tokio::spawn(prep.try_clone()
			.unwrap()
			.form(&body)
			.send());
		body = transform_body(body);
		futures.push(f);
		if i & chunk_size == 0 {
			if i != 0 {
				println!("waiting {} task to fulfill.", i);
				let result: Vec<_> = futures.collect().await;

				let err = result.into_iter().find(|it| {
					return if it.is_err() { true } else {
						let it = it.as_ref().unwrap();
						match it {
							Ok(resp) => {
								!resp.status().is_success()
							}
							Err(_) => {
								true
							}
						}
					};
				});
				if let Some(err) = err {
					println!("Found error stopping");
					println!("{:#?}", err);
					futures = FuturesUnordered::new();
					break;
				}
				futures = FuturesUnordered::new();
			}
			println!("spawning task {}", i + 1);
		}
		i += 1;
		if i == limit {
			break;
		}
		if rate != 0 {
			let diff = Instant::now().duration_since(last);
			if diff.as_secs() < 1 && (i % rate) == 0 && i != 0 {
				println!("Limiting {}req/s ({}ms)", rate, diff.as_millis());
				sleep(Duration::from_millis((1000 - diff.as_millis()) as u64)).await;
				last = Instant::now();
			}
		}
	};
	let result: Vec<_> = futures.collect().await;
	let err = result.into_iter().find(|it| it.is_err() || it.as_ref().unwrap().is_err());
	if let Some(err) = err {
		println!("Found error stopping");
		println!("{:#?}", err);
	}
}

#[derive(Debug)]
struct Site {
	url: String,
	referer: &'static str,
}