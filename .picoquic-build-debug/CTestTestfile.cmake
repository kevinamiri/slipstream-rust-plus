# CMake generated Testfile for 
# Source directory: /home/slipstream-rust-plus/vendor/picoquic
# Build directory: /home/slipstream-rust-plus/.picoquic-build-debug
# 
# This file includes the relevant testing commands required for 
# testing this directory and lists subdirectories to be tested as well.
add_test(picoquic_ct "/home/slipstream-rust-plus/.picoquic-build-debug/picoquic_ct" "-S" "/home/slipstream-rust-plus/vendor/picoquic" "-n" "-r")
set_tests_properties(picoquic_ct PROPERTIES  _BACKTRACE_TRIPLES "/home/slipstream-rust-plus/vendor/picoquic/CMakeLists.txt;444;add_test;/home/slipstream-rust-plus/vendor/picoquic/CMakeLists.txt;0;")
add_test(picohttp_ct "/home/slipstream-rust-plus/.picoquic-build-debug/picohttp_ct" "-S" "/home/slipstream-rust-plus/vendor/picoquic" "-n" "-r")
set_tests_properties(picohttp_ct PROPERTIES  _BACKTRACE_TRIPLES "/home/slipstream-rust-plus/vendor/picoquic/CMakeLists.txt;446;add_test;/home/slipstream-rust-plus/vendor/picoquic/CMakeLists.txt;0;")
subdirs("_deps/picotls-build")
