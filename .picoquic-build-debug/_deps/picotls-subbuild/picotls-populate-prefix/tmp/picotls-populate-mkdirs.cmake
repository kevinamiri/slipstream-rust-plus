# Distributed under the OSI-approved BSD 3-Clause License.  See accompanying
# file Copyright.txt or https://cmake.org/licensing for details.

cmake_minimum_required(VERSION 3.5)

file(MAKE_DIRECTORY
  "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-src"
  "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-build"
  "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix"
  "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/tmp"
  "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp"
  "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src"
  "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp"
)

set(configSubDirs )
foreach(subDir IN LISTS configSubDirs)
    file(MAKE_DIRECTORY "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/${subDir}")
endforeach()
if(cfgdir)
  file(MAKE_DIRECTORY "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp${cfgdir}") # cfgdir has leading slash
endif()
