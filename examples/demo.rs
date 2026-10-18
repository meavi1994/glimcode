// Place the cursor on `total` below and press F12 or Cmd/Ctrl B.
fn total(prices: &[f64]) -> f64 {
    prices.iter().sum()
}

fn main() {
    let prices = [89.50, 299.00, 4.50];
    println!("Total: {}", total(&prices));
}
