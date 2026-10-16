use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
    message::{Mailbox, header::ContentType},
};
use tokio::task::JoinHandle;

pub fn queue(
    smtp_url: String,
    from: String,
    to: String,
    subject: String,
    body: String,
) -> JoinHandle<Result<(), String>> {
    tokio::spawn(async move {
        let from: Mailbox = from
            .parse()
            .map_err(|error| format!("Invalid sender address: {error}"))?;
        let to: Mailbox = to
            .parse()
            .map_err(|error| format!("Invalid recipient address: {error}"))?;
        let message = Message::builder()
            .from(from)
            .to(to)
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .map_err(|error| format!("Could not build email: {error}"))?;
        let mailer = AsyncSmtpTransport::<Tokio1Executor>::from_url(&smtp_url)
            .map_err(|error| format!("Invalid SMTP connection URL: {error}"))?
            .build();

        mailer
            .send(message)
            .await
            .map(|_| ())
            .map_err(|error| format!("SMTP send failed: {error}"))
    })
}
