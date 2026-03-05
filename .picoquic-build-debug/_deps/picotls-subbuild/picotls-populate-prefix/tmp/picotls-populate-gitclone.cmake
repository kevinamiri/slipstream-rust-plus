# Distributed under the OSI-approved BSD 3-Clause License.  See accompanying
# file Copyright.txt or https://cmake.org/licensing for details.

cmake_minimum_required(VERSION 3.5)

if(EXISTS "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/picotls-populate-gitclone-lastrun.txt" AND EXISTS "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/picotls-populate-gitinfo.txt" AND
  "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/picotls-populate-gitclone-lastrun.txt" IS_NEWER_THAN "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/picotls-populate-gitinfo.txt")
  message(STATUS
    "Avoiding repeated git clone, stamp file is up to date: "
    "'/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/picotls-populate-gitclone-lastrun.txt'"
  )
  return()
endif()

execute_process(
  COMMAND ${CMAKE_COMMAND} -E rm -rf "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-src"
  RESULT_VARIABLE error_code
)
if(error_code)
  message(FATAL_ERROR "Failed to remove directory: '/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-src'")
endif()

# try the clone 3 times in case there is an odd git clone issue
set(error_code 1)
set(number_of_tries 0)
while(error_code AND number_of_tries LESS 3)
  execute_process(
    COMMAND "/usr/bin/git"
            clone --no-checkout --config "advice.detachedHead=false" "https://github.com/h2o/picotls.git" "picotls-src"
    WORKING_DIRECTORY "/home/slipstream-rust-plus/.picoquic-build-debug/_deps"
    RESULT_VARIABLE error_code
  )
  math(EXPR number_of_tries "${number_of_tries} + 1")
endwhile()
if(number_of_tries GREATER 1)
  message(STATUS "Had to git clone more than once: ${number_of_tries} times.")
endif()
if(error_code)
  message(FATAL_ERROR "Failed to clone repository: 'https://github.com/h2o/picotls.git'")
endif()

execute_process(
  COMMAND "/usr/bin/git"
          checkout "5a4461d8a3948d9d26bf861e7d90cb80d8093515" --
  WORKING_DIRECTORY "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-src"
  RESULT_VARIABLE error_code
)
if(error_code)
  message(FATAL_ERROR "Failed to checkout tag: '5a4461d8a3948d9d26bf861e7d90cb80d8093515'")
endif()

set(init_submodules TRUE)
if(init_submodules)
  execute_process(
    COMMAND "/usr/bin/git" 
            submodule update --recursive --init 
    WORKING_DIRECTORY "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-src"
    RESULT_VARIABLE error_code
  )
endif()
if(error_code)
  message(FATAL_ERROR "Failed to update submodules in: '/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-src'")
endif()

# Complete success, update the script-last-run stamp file:
#
execute_process(
  COMMAND ${CMAKE_COMMAND} -E copy "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/picotls-populate-gitinfo.txt" "/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/picotls-populate-gitclone-lastrun.txt"
  RESULT_VARIABLE error_code
)
if(error_code)
  message(FATAL_ERROR "Failed to copy script-last-run stamp file: '/home/slipstream-rust-plus/.picoquic-build-debug/_deps/picotls-subbuild/picotls-populate-prefix/src/picotls-populate-stamp/picotls-populate-gitclone-lastrun.txt'")
endif()
