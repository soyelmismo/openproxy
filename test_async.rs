use std::future::Future;

pub trait ProxyBackend {
    fn handle_request(&self) -> impl Future<Output = Result<(), ()>> + Send;
}

pub struct MyBackend;

impl ProxyBackend for MyBackend {
    async fn handle_request(&self) -> Result<(), ()> {
        Ok(())
    }
}
