use uuid::Uuid;

#[derive(Debug)]
enum PaymentStatus {
    Pending,
    Confirmed,
    Failed(String),
}

#[derive(Debug)]
struct Payment {
    id: String,
    from: String,
    to: String,
    amount: f64,
    currency: String,
    status: PaymentStatus,
}

impl Payment {
    fn new(from: &str, to: &str, amount: f64, currency: &str) -> Self {
        Payment {
            id: Uuid::new_v4().to_string(),
            from: from.to_string(),
            to: to.to_string(),
            amount,
            currency: currency.to_string(),
            status: PaymentStatus::Pending,
        }
    }

    fn process(&mut self) {
        if self.amount <= 0.0 {
            self.status = PaymentStatus::Failed("Amount must be greater than zero".to_string());
            return;
        }
        if self.from.is_empty() || self.to.is_empty() {
            self.status = PaymentStatus::Failed("Invalid sender or recipient".to_string());
            return;
        }
        self.status = PaymentStatus::Confirmed;
    }

    fn summary(&self) {
        let status = match &self.status {
            PaymentStatus::Pending => "Pending".to_string(),
            PaymentStatus::Confirmed => "Confirmed".to_string(),
            PaymentStatus::Failed(reason) => format!("Failed: {reason}"),
        };
        println!(
            "[{}] {} -> {} | {} {} | {}",
            &self.id[..8],
            self.from,
            self.to,
            self.amount,
            self.currency,
            status
        );
    }
}

fn main() {
    println!("Nodus Protocol — Core Engine\n");

    let mut payment = Payment::new("0xAlice", "0xBob", 50.0, "USDC");
    payment.process();
    payment.summary();

    let mut bad_payment = Payment::new("0xAlice", "0xBob", -10.0, "USDC");
    bad_payment.process();
    bad_payment.summary();
}
