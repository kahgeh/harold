pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("harold_descriptor");

pub mod harold {
    tonic::include_proto!("harold");
}

pub mod tools {
    pub mod questions {
        pub mod v1 {
            tonic::include_proto!("harold.tools.questions.v1");
        }
    }
}
