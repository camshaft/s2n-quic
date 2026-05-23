// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

pub fn spawn(counters: s2n_quic_dc::counter::Registry) {
    s2n_quic_dc::endpoint::counters::os::spawn(counters);
}
