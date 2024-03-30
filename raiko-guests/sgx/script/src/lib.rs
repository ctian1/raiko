#![cfg(feature = "enable")]
use std::{
    env, fs::{copy, create_dir_all, remove_file, File}, path::{Path, PathBuf}, process::Output, str
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_with::serde_as;
use tokio::{process::Command, sync::OnceCell};
use tracing::{debug, info};
use raiko_lib::input::{GuestInput, GuestOutput};
use once_cell::sync::Lazy;

#[serde_as]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SgxParam {
    pub instance_id: u64,
    pub input_path: Option<PathBuf>,
}

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SgxResponse {
    /// proof format: 4b(id)+20b(pubkey)+65b(signature)
    pub proof: String,
    pub quote: String,
}

pub const INPUT_FILE_NAME: &str = "input.bin";
static GRAMINE_MANIFEST_TEMPLATE: Lazy<OnceCell<PathBuf>> = Lazy::new(OnceCell::new);
static PRIVATE_KEY: Lazy<OnceCell<PathBuf>> = Lazy::new(OnceCell::new);

async fn prepare_working_directory(direct_mode: bool) -> PathBuf {
    let cur_dir = env::current_exe()
        .expect("Fail to get current directory")
        .parent()
        .unwrap()
        .to_path_buf();

    // Create required directories
    let directories = ["secrets", "config"];
    for dir in directories {
        create_dir_all(cur_dir.join(dir)).unwrap();
    }
    GRAMINE_MANIFEST_TEMPLATE
        .set(cur_dir.join("config").join("raiko-guest.manifest.template"))
        .expect("Fail to set GRAMINE_MANIFEST_TEMPLATE");
    PRIVATE_KEY.get_or_init( || async {
        // Bootstrap
        // First delete the private key if it already exists
        let path = cur_dir.join("secrets").join("priv.key");
        if path.exists() {
            if let Err(e) = remove_file(&path) {
                println!("Error deleting file: {}", e);
            }
        }
        path
    }).await;
    if direct_mode {
        // Copy dummy files
        let files = ["attestation_type", "quote", "user_report_data"];
        for file in files {
            copy(
                cur_dir.join("config").join("dummy_data").join(file),
                cur_dir.join(file),
            )
            .unwrap();
        }
    }
    cur_dir
}

pub async fn execute(
    input: GuestInput,
    _output: GuestOutput,
    param: &SgxParam,
) -> Result<SgxResponse, String> {
    

    // Support both SGX and the direct backend for testing
    let direct_mode = match env::var("SGX_DIRECT") {
        Ok(value) => value == "1",
        Err(_) => false,
    };
    // Print a warning when running in direct mode
    if direct_mode {
        println!("WARNING: running SGX in direct mode!");
    }

    // Working paths
    let cur_dir = prepare_working_directory(direct_mode).await;

     // If cached input file is not provided
     // write the input to a file that will be read by the SGX instance
    let bin = match &param.input_path {
        Some(path) => path.clone(),
        None => {
            let path = cur_dir.join(INPUT_FILE_NAME);
            bincode::serialize_into(
                File::create(&path).expect("Unable to open file"),
                &input).expect("Unable to serialize input"
            );
            path
        }
    };

    // Generate the manifest
    let mut cmd = Command::new("gramine-manifest");
    let output = cmd
        .current_dir(cur_dir.clone())
        .arg("-Dlog_level=error")
        .arg("-Darch_libdir=/lib/x86_64-linux-gnu/")
        .arg(format!(
            "-Ddirect_mode={}",
            if direct_mode { "1" } else { "0" }
        ))
        .arg(GRAMINE_MANIFEST_TEMPLATE.get().unwrap())
        .arg("sgx-guest.manifest")
        .output()
        .await
        .map_err(|e| format!("Could not generate manfifest: {}", e.to_string()))?;

    print_output(&output, "Generate manifest");

    if !direct_mode {
        // Generate a private key
        let mut cmd = Command::new("gramine-sgx-gen-private-key");
        cmd.current_dir(cur_dir.clone())
            .arg("-f")
            .output()
            .await
            .map_err(|e| format!("Could not generate SGX private key: {}", e.to_string()))?;

        // Sign the manifest
        let mut cmd = Command::new("gramine-sgx-sign");
        cmd.current_dir(cur_dir.clone())
            .arg("--manifest")
            .arg("sgx-guest.manifest")
            .arg("--output")
            .arg("sgx-guest.manifest.sgx")
            .output()
            .await
            .map_err(|e| format!("Could not sign manfifest: {}", e.to_string()))?;
    }

    // Form gramine command
    let gramine_cmd = || -> Command {
        let mut cmd = if direct_mode {
            Command::new("gramine-direct")
        } else {
            let mut cmd = Command::new("sudo");
            cmd.arg("gramine-sgx");
            cmd
        };
        cmd.current_dir(&cur_dir).arg(&bin);
        cmd
    };

    // Bootstrap new private key
    let output = gramine_cmd()
        .arg("bootstrap")
        .output()
        .await
        .map_err(|e| format!("Could not run SGX guest boostrap: {}", e.to_string()))?;
    print_output(&output, "Sgx bootstrap");

    // Prove
    let output = gramine_cmd()
        .arg("one-shot")
        .arg("--sgx-instance-id")
        .arg(param.instance_id.to_string())
        .output()
        .await
        .map_err(|e| format!("Could not run SGX guest prover: {}", e.to_string()))?;
    print_output(&output, "Sgx execution");

    if !output.status.success() {
        // inc_sgx_error(req.block_number);
        return Err(output.status.to_string());
    }

    parse_sgx_result(output.stdout)
}

fn parse_sgx_result(output: Vec<u8>) -> Result<SgxResponse, String> {
    let mut json_value: Option<Value> = None;
    let output = String::from_utf8(output).map_err(|e| e.to_string())?;

    for line in output.lines() {
        if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
            json_value = Some(value);
            break;
        }
    }
    let extract_field = |field| {
        json_value
            .as_ref()
            .and_then(|json| json.get(field).and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string()
    };
    let proof = extract_field("proof");
    let quote = extract_field("quote");

    Ok(SgxResponse { proof, quote })
}


fn print_output(output: &Output, name: &str) {
    print!("{} stderr: {}\n", name, str::from_utf8(&output.stderr).unwrap());
    print!("{} stdout: {}\n", name,str::from_utf8(&output.stdout).unwrap());

}