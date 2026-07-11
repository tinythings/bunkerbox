# Bunkerbox

Bunkerbox runs developer tools inside an isolated container instead of running them directly on your machine.

The idea is simple. Many modern developer tools need to read and edit files in your project. Some of them may also talk to the network, save state, or run helper commands. That is useful, but it also means the tool can touch more of your system than you may want. Bunkerbox gives the tool a smaller place to work.

When a tool runs through Bunkerbox, it sees your project as `/workspace`. Its app home is separated from your real home directory. Its image is built from a config file. Its runtime behavior is described by another config file. This makes the tool feel like a normal command, while still putting a boundary around it.

## The basic flow

First, you build an image from an image config. The image contains the tool and the generated Bunkerbox entrypoint.

```sh
make image IMAGE=images/opencode.conf
```

Then, for local development, you import the OCI archive that was produced by the image build.

```sh
make install-image OCI=bunkerbox-opencode-1.17.18.oci
```

In a packaged install, the user does not need to think about that internal flow. They can run a command such as `opencode`, and Bunkerbox loads the matching runtime config for that command.

## Why this exists

Developer agents are getting stronger. They can edit code, inspect projects, call tools, and keep state. Running them directly on the host is convenient, but it gives them the same local access as the user who launched them.

Bunkerbox explores a safer model. The tool still gets the project workspace it needs, but the runtime is prepared, isolated, and described explicitly.

## Documentation website

Build the documentation website with:

```sh
make docs
```

The static site is written to:

```text
target/site-docs/
```

## Where to go next

Start with the [Quickstart](quickstart.md) if you want to run the development flow. Read [Concepts](concepts.md) if you want to understand the model. Read [Packaging](guides/packaging.md) if you want to understand how a tool becomes a normal command.
