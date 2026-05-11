# Bedina

Integration examples for **BE**vy and **DI**oxus **NA**tive (and potentially in future a library).

Bevy is a game engine with 3D rendering capabilities. Dioxus Native is an HTML/CSS renderer with a Rust-based reactivity framework (Dioxus) on top. Both Bevy and Dioxus Native render using wgpu, so the there is potential to render 3D content with Bevy with UI rendered using Dioxus Native.

This repo contains examples of two different ways of doing this:

- The `bevy-in-dioxus-native` example has Dioxus Native as the top-level entity which owns the window and event loop. In this mode, Bevy renders itself to texture using a wgpu Device provided to it by Blitz inside a Blitz custom widget.

- The `dioxus-native-in-bevy` example has Bevy as the top-level entity which owns the window and event loop. In this mode, Dioxus Native renders itself to texture using a wgpu Device provided to it by Bevy.
