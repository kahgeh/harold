pub use prost;

pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("harold_descriptor");

pub mod harold {
    tonic::include_proto!("harold");

    // Nested here so the generated `super::super::AgentStateSnapshot` resolves.
    pub mod slave {
        pub mod v1 {
            tonic::include_proto!("harold.slave.v1");
        }
    }
}

pub mod tts {
    pub mod v1 {
        tonic::include_proto!("harold.tts.v1");
    }
}

pub mod tools {
    pub mod backlog {
        pub mod v1 {
            tonic::include_proto!("harold.tools.backlog.v1");
        }
    }
    pub mod job {
        pub mod v1 {
            tonic::include_proto!("harold.tools.job.v1");
        }
    }
    pub mod knowledge {
        pub mod v1 {
            tonic::include_proto!("harold.tools.knowledge.v1");
        }
    }
    pub mod questions {
        pub mod v1 {
            tonic::include_proto!("harold.tools.questions.v1");
        }
    }
    pub mod evidence {
        pub mod v1 {
            tonic::include_proto!("harold.tools.evidence.v1");
        }
    }
    pub mod release {
        pub mod v1 {
            tonic::include_proto!("harold.tools.release.v1");
        }
    }
    pub mod telemetry {
        pub mod v1 {
            tonic::include_proto!("harold.tools.telemetry.v1");
        }
    }
    pub mod watch {
        pub mod v1 {
            tonic::include_proto!("harold.tools.watch.v1");
        }
    }
    pub mod work {
        // The generated code trips a clippy lint that the older generated modules also trip.
        // This module adds no new warning.
        #[allow(clippy::double_must_use)]
        pub mod v1 {
            tonic::include_proto!("harold.tools.work.v1");
        }
    }
}
