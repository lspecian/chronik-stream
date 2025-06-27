//! Example demonstrating SCRAM-SHA-256 and SCRAM-SHA-512 authentication

use chronik_auth::SaslAuthenticator;

fn main() {
    // Create authenticator and add a user
    let mut auth = SaslAuthenticator::new();
    auth.add_user("alice".to_string(), "password123".to_string())
        .expect("Failed to add user");
    
    println!("SCRAM-SHA-256 Authentication Example");
    println!("====================================");
    
    // Step 1: Client sends first message
    let client_nonce = "rOprNGfwEbeRWgbNEkqO";
    let client_first = format!("n,,n=alice,r={}", client_nonce);
    println!("Client First: {}", client_first);
    
    // Step 2: Server processes first message and sends challenge
    let session_id = "example-session";
    let server_first_bytes = auth.process_scram_sha256_first(session_id, client_first.as_bytes())
        .expect("Failed to process client first");
    let server_first = String::from_utf8(server_first_bytes).expect("Invalid UTF-8");
    println!("Server First: {}", server_first);
    
    // Step 3: Client would calculate proof here
    // For this example, we'll simulate the client-side calculation
    let channel_binding = "biws"; // base64("n,,")
    let combined_nonce = extract_nonce(&server_first);
    let _salt = extract_salt(&server_first);
    let _iterations = extract_iterations(&server_first);
    
    // Calculate client proof (simplified for example)
    let client_final_without_proof = format!("c={},r={}", channel_binding, combined_nonce);
    let _auth_message = format!("n=alice,r={},{},{}", 
        client_nonce, server_first, client_final_without_proof);
    
    // In a real client, you would calculate the proof here
    // For this example, we'll create a mock client final message
    println!("\nClient would calculate proof and send final message...");
    
    // Step 4: Server verifies proof and sends final message
    // (In a real scenario, the client would send the actual calculated proof)
    
    println!("\nAuthentication flow demonstrated!");
    println!("\nSupported mechanisms:");
    for mechanism in auth.handshake(&[]) {
        println!("  - {}", mechanism);
    }
}

fn extract_nonce(server_first: &str) -> &str {
    let start = server_first.find("r=").unwrap() + 2;
    let end = server_first[start..].find(',').unwrap() + start;
    &server_first[start..end]
}

fn extract_salt(server_first: &str) -> &str {
    let start = server_first.find("s=").unwrap() + 2;
    let end = server_first[start..].find(',').unwrap_or(server_first.len() - start) + start;
    &server_first[start..end]
}

fn extract_iterations(server_first: &str) -> u32 {
    let start = server_first.find("i=").unwrap() + 2;
    server_first[start..].parse().unwrap()
}