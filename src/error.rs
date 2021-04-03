use failure_derive::Fail;

#[derive(Fail, Debug)]
#[fail(display = "Needs a Home Directory")]
pub struct NeedHomeDir;
