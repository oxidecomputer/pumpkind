extern "C" {
    pub fn bind_to_iface(socket: i32, ifindex: u32) -> ::std::os::raw::c_int;
}

pub fn bind_to_interface(
    sockfd: &impl std::os::fd::AsRawFd,
    ifindex: u32,
) -> anyhow::Result<()> {
    let fd = sockfd.as_raw_fd();
    match unsafe { bind_to_iface(fd, ifindex) } {
        0 => Ok(()),
        x => Err(anyhow::anyhow!("failed to bind socket: errno={x}")),
    }
}
