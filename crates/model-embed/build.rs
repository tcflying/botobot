//! 构建脚本（§记忆语义召回·内嵌方案）：构建时从 HuggingFace 拉 bge-small-zh-v1.5 的
//! 权重/分词器/配置，把 fp32 权重转 fp16 缓存到 crate 本地 `.modelcache/`（gitignore，**不进 git**），
//! 路径经 `cargo:rustc-env=BGE_DIR` 暴露给 `lib.rs` 的 `include_bytes!`/`include_str!`。
//!
//! 净效果 = 最终二进制 `include_bytes!` 自包含 fp16 权重（离线可跑），但 git 不增 ~48MB blob；
//! 首次构建联网下载一次（~96MB fp32），之后命中缓存。离线且无缓存 → 构建失败并提示。
//! 借鉴前身 datoobot `embed-tech/build.rs`。

use std::path::{Path, PathBuf};

const REPO: &str = "BAAI/bge-small-zh-v1.5/resolve/main";
const MIRRORS: &[&str] = &["https://hf-mirror.com", "https://huggingface.co"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=EMBED_MODEL_DIR");

    // 离线/CI：把预下的三文件放某目录，指 EMBED_MODEL_DIR 即可跳过下载。
    if let Ok(dir) = std::env::var("EMBED_MODEL_DIR") {
        let dir = PathBuf::from(dir);
        for f in ["config.json", "tokenizer.json", "model.f16.safetensors"] {
            assert!(
                dir.join(f).exists(),
                "EMBED_MODEL_DIR={} 缺 {f}",
                dir.display()
            );
        }
        println!("cargo:rustc-env=BGE_DIR={}", dir.display());
        return;
    }

    let cache = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join(".modelcache");
    std::fs::create_dir_all(&cache).expect("创建 .modelcache 失败");

    fetch_if_missing(&cache.join("config.json"), "config.json");
    fetch_if_missing(&cache.join("tokenizer.json"), "tokenizer.json");

    let f16_path = cache.join("model.f16.safetensors");
    if !f16_path.exists() {
        let f32_path = cache.join("model.f32.safetensors");
        fetch_if_missing(&f32_path, "model.safetensors");
        convert_f32_to_f16(&f32_path, &f16_path);
    }

    println!("cargo:rustc-env=BGE_DIR={}", cache.display());
}

fn fetch_if_missing(dst: &Path, rel: &str) {
    if dst.exists() {
        return;
    }
    let mut last_err = String::new();
    for base in MIRRORS {
        let url = format!("{base}/{REPO}/{rel}");
        eprintln!("model-embed build.rs: 下载 {url}");
        match ureq::get(&url)
            .timeout(std::time::Duration::from_secs(180))
            .call()
        {
            Ok(resp) => {
                let tmp = dst.with_extension("part");
                let mut out = std::fs::File::create(&tmp).expect("创建临时下载文件失败");
                if let Err(e) = std::io::copy(&mut resp.into_reader(), &mut out) {
                    last_err = format!("{url}: 写入失败 {e}");
                    continue;
                }
                drop(out);
                std::fs::rename(&tmp, dst).expect("重命名下载文件失败");
                return;
            }
            Err(e) => last_err = format!("{url}: {e}"),
        }
    }
    panic!(
        "下载 {rel} 失败（所有镜像不可达）：{last_err}\n\
         若构建机无法访问 HuggingFace，请手动下 config.json/tokenizer.json/model.safetensors\
         （转 fp16 为 model.f16.safetensors），放一目录后设 EMBED_MODEL_DIR=<该目录> 再构建。"
    );
}

fn convert_f32_to_f16(src: &Path, dst: &Path) {
    use safetensors::{Dtype, SafeTensors, tensor::TensorView};
    let raw = std::fs::read(src).expect("读 fp32 权重失败");
    let st = SafeTensors::deserialize(&raw).expect("解析 fp32 safetensors 失败");
    let mut bufs: Vec<(String, Dtype, Vec<usize>, Vec<u8>)> = Vec::new();
    for (name, view) in st.tensors() {
        let shape = view.shape().to_vec();
        if view.dtype() == Dtype::F32 {
            let data = view.data();
            let mut out = Vec::with_capacity(data.len() / 2);
            for c in data.chunks_exact(4) {
                let f = f32::from_le_bytes([c[0], c[1], c[2], c[3]]);
                out.extend_from_slice(&half::f16::from_f32(f).to_le_bytes());
            }
            bufs.push((name.to_string(), Dtype::F16, shape, out));
        } else {
            bufs.push((name.to_string(), view.dtype(), shape, view.data().to_vec()));
        }
    }
    let views: Vec<(String, TensorView)> = bufs
        .iter()
        .map(|(n, d, s, b)| (n.clone(), TensorView::new(*d, s.clone(), b).unwrap()))
        .collect();
    let bytes = safetensors::serialize(views, &None).expect("序列化 fp16 safetensors 失败");
    std::fs::write(dst, bytes).expect("写 fp16 权重失败");
}
