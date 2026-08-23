pub const FILE_DESCRIPTOR_SET: &[u8] = tonic::include_file_descriptor_set!("harold_descriptor");

pub mod harold {
    tonic::include_proto!("harold");
}
