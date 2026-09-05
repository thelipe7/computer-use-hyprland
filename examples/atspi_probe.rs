//! Print the real error chain behind a failed AT-SPI connection, which is
//! the one diagnostic `doctor` reduces to a yes or a no.

// Scratch probe: surface the real AT-SPI connection error chain.
use atspi_connection::AccessibilityConnection;

#[tokio::main]
async fn main() {
    println!(
        "AT_SPI_BUS_ADDRESS={:?}",
        std::env::var("AT_SPI_BUS_ADDRESS")
    );
    println!(
        "DBUS_SESSION_BUS_ADDRESS={:?}",
        std::env::var("DBUS_SESSION_BUS_ADDRESS")
    );
    match AccessibilityConnection::new().await {
        Ok(conn) => {
            println!("connected OK");
            match conn.root_accessible_on_registry().await {
                Ok(root) => match root.get_children().await {
                    Ok(children) => println!("registry children: {}", children.len()),
                    Err(e) => println!("get_children error: {e:#?}"),
                },
                Err(e) => println!("registry root error: {e:#?}"),
            }
        }
        Err(e) => println!("connect error: {e:#?}"),
    }
}
