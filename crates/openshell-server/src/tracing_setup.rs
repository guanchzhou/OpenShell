// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Process-wide tracing subscriber setup for the gateway.
//!
//! This module routes gateway logs and spans to configured diagnostic outputs.
//! `OpenShell` product telemetry collected for maintainers is handled by
//! [`crate::telemetry`].

use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::prelude::*;

use crate::ConfiguredComputeDriver;
use crate::config_file::OtlpConfig;
use crate::otel_tracing::{GatewayResourceAttributes, SetupError};
use crate::tracing_bus::TracingLogBus;

pub struct TracingHandle {
    tracer_provider: Option<SdkTracerProvider>,
    driver_tracer_provider: Option<SdkTracerProvider>,
}

impl TracingHandle {
    pub fn shutdown(&self) {
        if let Some(provider) = &self.tracer_provider
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "OTLP tracer provider shutdown failed");
        }
        if let Some(provider) = &self.driver_tracer_provider
            && let Err(err) = provider.shutdown()
        {
            tracing::warn!(error = %err, "compute-driver OTLP tracer provider shutdown failed");
        }
    }
}

fn in_process_driver_tracing(
    driver: &ConfiguredComputeDriver,
) -> Option<openshell_otel::ComputeDriverTracing> {
    match driver {
        ConfiguredComputeDriver::Registered(registration) => registration.in_process_tracing(),
        ConfiguredComputeDriver::Remote { .. } => None,
    }
}

fn in_process_driver_provider(
    driver: Option<openshell_otel::ComputeDriverTracing>,
    endpoint: Option<&str>,
    gateway: GatewayResourceAttributes<'_>,
) -> (Option<SdkTracerProvider>, Option<SetupError>) {
    driver.map_or_else(
        || (None, None),
        |descriptor| {
            descriptor.provider_for(
                endpoint,
                openshell_core::VERSION,
                gateway.name(),
                gateway.compute_driver(),
            )
        },
    )
}

fn in_process_driver_layer<S>(
    provider: &Option<SdkTracerProvider>,
    driver: Option<openshell_otel::ComputeDriverTracing>,
) -> Option<openshell_otel::TargetOtlpLayer<S>>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    provider.as_ref().map(|provider| {
        driver
            .expect("a driver provider requires a selected driver")
            .in_process_layer(provider)
    })
}

pub fn install(
    env_filter: EnvFilter,
    tracing_log_bus: &TracingLogBus,
    otlp_config: Option<&OtlpConfig>,
    driver: &ConfiguredComputeDriver,
    gateway: GatewayResourceAttributes<'_>,
) -> (TracingHandle, Option<SetupError>) {
    let (tracer_provider, setup_error) = crate::otel_tracing::provider_for(otlp_config, gateway);
    let selected_driver = in_process_driver_tracing(driver);
    let driver_endpoint = selected_driver
        .is_some()
        .then_some(otlp_config)
        .flatten()
        .map(|config| config.endpoint.as_str());
    let (driver_tracer_provider, driver_setup_error) =
        in_process_driver_provider(selected_driver, driver_endpoint, gateway);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer())
        .with(tracing_log_bus.layer())
        .with(
            tracer_provider.as_ref().map(|provider| {
                crate::otel_tracing::layer_excluding_driver(provider, selected_driver)
            }),
        )
        .with(in_process_driver_layer(
            &driver_tracer_provider,
            selected_driver,
        ))
        .init();

    (
        TracingHandle {
            tracer_provider,
            driver_tracer_provider,
        },
        setup_error.or(driver_setup_error),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(not(target_os = "windows"), feature = "in-tree-compute-drivers"))]
    #[test]
    fn in_process_driver_tracing_selects_registered_compute_drivers() {
        let registry = crate::install_default_compute_drivers();
        let registered = |name| {
            ConfiguredComputeDriver::Registered(
                registry
                    .get(name)
                    .unwrap_or_else(|| panic!("{name} driver is registered"))
                    .clone(),
            )
        };
        assert_eq!(
            in_process_driver_tracing(&registered("podman")),
            Some(openshell_driver_podman::otel_tracing::TRACING)
        );
        assert_eq!(
            in_process_driver_tracing(&registered("docker")),
            Some(openshell_driver_docker::otel_tracing::TRACING)
        );
        assert_eq!(
            in_process_driver_tracing(&registered("kubernetes")),
            Some(openshell_driver_kubernetes::otel_tracing::TRACING)
        );
        for name in ["podman", "docker", "kubernetes"] {
            let descriptor = in_process_driver_tracing(&registered(name))
                .unwrap_or_else(|| panic!("{name} registers an in-process descriptor"));
            assert_eq!(
                descriptor.compute_driver(),
                name,
                "{name} must register its own descriptor"
            );
        }
        assert_eq!(in_process_driver_tracing(&registered("vm")), None);
        assert_eq!(
            in_process_driver_tracing(&ConfiguredComputeDriver::Remote {
                name: "custom".to_string(),
            }),
            None
        );
    }
}
