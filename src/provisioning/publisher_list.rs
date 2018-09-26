//! Responsible for storing and retrieving Publisher information.
use std::str;
use rpki::oob::exchange::PublisherRequest;
use rpki::remote::idcert::IdCert;
use rpki::uri;


//------------ Publisher -----------------------------------------------------

/// This type defines Publisher CAs that are allowed to publish.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Publisher {
    name:       String,
    base_uri:   uri::Rsync,
    id_cert:    IdCert
}


//------------ Command -------------------------------------------------------

// These are the commands to send to the PublisherList that allow updating the
// list of Publishers.
#[derive(Debug)]
pub enum Command {
    Add(PublisherRequest),
    Remove(String)
}

#[derive(Debug)]
pub struct VersionedCommand {
    version: usize,
    command: Command
}

impl VersionedCommand {
    pub fn add_publisher(
        version: usize,
        pr: PublisherRequest
    ) -> Self {
        VersionedCommand {
            version,
            command: Command::Add(pr)
        }
    }

    pub fn remove_publisher(
        version: usize,
        name: String
    ) -> Self {
        VersionedCommand {
            version,
            command: Command::Remove(name)
        }
    }
}

//------------ Event ---------------------------------------------------------

// These are the events that occurred on the PublisherList. Together they
// form a complete audit trail, and when replayed in order will result in
// the current state of the PublisherList.
#[derive(Clone, Debug)]
pub enum Event {
    Added(PublisherAdded),
    CertUpdated(PublisherIdUpdated),
    Removed(PublisherRemoved)
}

#[derive(Clone, Debug)]
pub struct VersionedEvent {
    version: usize,
    event: Event
}

#[derive(Clone, Debug)]
pub struct PublisherAdded(Publisher);

#[derive(Clone, Debug)]
pub struct PublisherIdUpdated(String);

#[derive(Clone, Debug)]
pub struct PublisherRemoved(String);


//------------ PublisherList -------------------------------------------------

#[derive(Debug)]
pub struct PublisherList {
    /// The version of this list. This gets updated with every modification.
    version: usize,

    /// The base URI for this repository server. Publishers will get a
    /// directory below this based on their 'publisher_handle'.
    base_uri: uri::Rsync,

    /// The current configured publishers.
    publishers: Vec<Publisher>
}


impl PublisherList {

    pub fn new(base_uri: uri::Rsync) -> Self {
        PublisherList {
            version: 0,
            base_uri,
            publishers: Vec::new()
        }
    }

    fn apply_event(
        &mut self,
        event: &VersionedEvent
    ) -> Result<(), Error> {

        if self.version != event.version {
            return Err(Error::VersionConflict(self.version, event.version))
        }

        let event = event.event.clone();

        match event {
            Event::Added(a)   => {
                let publisher = a.0;
                if self.has_publisher(&publisher.name) {
                    return Err(Error::DuplicatePublisher(publisher.name))
                }
                self.publishers.push(publisher)
            },
            Event::Removed(r) => {
                let name = r.0;
                if ! self.has_publisher(&name) {
                    return Err(Error::UnknownPublisher(name))
                }
                self.publishers.retain(|p| { p.name != name })
            },
            _ => unimplemented!()
        }

        self.version = self.version + 1;
        Ok(())
    }

    fn has_publisher(&self, name: &String) -> bool {
        self.publishers.iter().find(|p| &p.name == name).is_some()
    }

    pub fn apply_command(
        &mut self,
        command: VersionedCommand
    ) -> Result<VersionedEvent, Error> {

        if self.version != command.version {
            return Err(
                Error::VersionConflict(
                    self.version, command.version))
        }

        match command.command {
            Command::Add(pub_req) => self.add_publisher(pub_req),
            Command::Remove(name) => self.remove_publisher(name)
        }
    }

    fn add_publisher(
        &mut self,
        pr: PublisherRequest
    ) -> Result<VersionedEvent, Error> {

        let (_, name, id_cert) = pr.into_parts();

        if name.contains("/") {
            return Err(
                Error::ForwardSlashInHandle(name))
        }

        let mut base_uri = self.base_uri.to_string();
        base_uri.push_str(name.as_ref());
        let base_uri = uri::Rsync::from_string(base_uri)?;

        let publisher = Publisher { name, base_uri, id_cert };

        let event = VersionedEvent {
            version: self.version,
            event: Event::Added(PublisherAdded(publisher))
        };

        self.apply_event(&event)?;

        Ok(event)
    }

    fn remove_publisher(
        &mut self,
        name: String
    ) -> Result<VersionedEvent, Error> {
        let event = VersionedEvent {
            version: self.version,
            event: Event::Removed(PublisherRemoved(name))
        };

        self.apply_event(&event)?;
        Ok(event)
    }

}


//------------ PublisherListError --------------------------------------------

#[derive(Debug, Fail)]
pub enum Error {

    #[fail(display =
        "Version conflict. Current version is: {}, update has: {}", _0, _1)]
    VersionConflict(usize, usize),

    #[fail(display =
        "The '/' in publisher_handle ({}) is not supported - because we \
        are deriving the base directory for a publisher from this. This \
        behaviour may be updated in future.", _0)]
    ForwardSlashInHandle(String),

    #[fail(display = "Error in base URI: {}.", _0)]
    UriError(uri::Error),

    #[fail(display = "Duplicate publisher with name: {}.", _0)]
    DuplicatePublisher(String),

    #[fail(display = "Unknown publisher with name: {}.", _0)]
    UnknownPublisher(String)
}

impl From<uri::Error> for Error {
    fn from(e: uri::Error) -> Self {
        Error::UriError(e)
    }
}


//------------ Tests ---------------------------------------------------------

#[cfg(test)]
mod tests {

    use super::*;
    use rpki::signing::signer::Signer;
    use rpki::signing::softsigner::OpenSslSigner;
    use rpki::signing::PublicKeyAlgorithm;
    use rpki::signing::builder::IdCertBuilder;

    fn rsync_uri(s: &str) -> uri::Rsync {
        uri::Rsync::from_str(s).unwrap()
    }

    fn empty_publisher_list() -> PublisherList {
        let base_uri = rsync_uri("rsync://host/module/");
        PublisherList::new(base_uri)
    }

    fn new_id_cert() -> IdCert {
        let mut s = OpenSslSigner::new();
        let key_id = s.create_key(&PublicKeyAlgorithm::RsaEncryption).unwrap();
        IdCertBuilder::new_ta_id_cert(&key_id, &mut s).unwrap()
    }

    #[test]
    fn should_add_publisher() {
        let mut cl = empty_publisher_list();
        let id_cert = new_id_cert();

        let pr = PublisherRequest::new(
            Some("test"),
            "test",
            id_cert.clone());

        let cmd = VersionedCommand::add_publisher(0, pr);
        cl.apply_command(cmd).unwrap();

        assert_eq!(1, cl.publishers.len());
        let publisher = cl.publishers.get(0).unwrap();
        let expected_publisher = Publisher {
            name: "test".to_string(),
            base_uri: rsync_uri("rsync://host/module/test"),
            id_cert
        };

        assert_eq!(publisher, &expected_publisher);
    }

    #[test]
    fn should_remove_publisher() {
        let mut cl = empty_publisher_list();
        let id_cert = new_id_cert();

        let pr = PublisherRequest::new(
            Some("test"),
            "test",
            id_cert.clone());

        let cmd = VersionedCommand::add_publisher(0, pr);
        cl.apply_command(cmd).unwrap();

        assert_eq!(1, cl.publishers.len());

        let cmd = VersionedCommand::remove_publisher(1, "test".to_string());
        cl.apply_command(cmd).unwrap();

        assert_eq!(0, cl.publishers.len());
    }

    #[test]
    fn should_refuse_slash_in_publisher_handle() {
        let mut cl = empty_publisher_list();
        let id_cert = new_id_cert();

        let pr = PublisherRequest::new(
            Some("test"),
            "test/below",
            id_cert);

        let cmd = VersionedCommand::add_publisher(0, pr);
        match cl.apply_command(cmd) {
            Err(Error::ForwardSlashInHandle(_)) => { }, // Ok
            _ => panic!("Should have seen error.")
        }
    }


}