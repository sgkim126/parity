// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

use error::SymmError;
use ring;

enum Key<'a> {
	Aes128Gcm(&'a [u8; 16]),
	Aes256Gcm(&'a [u8; 32]),
}

pub struct Builder<'a> {
	key: Key<'a>,
	nonce: &'a [u8; 12],
	ad: &'a [u8],
	offset: usize,
}

impl<'a> Builder<'a> {
	/// AES-128 GCM mode encryption.
	///
	/// NOTE: The pair (key, nonce) must never be reused. Using random nonces limits
	/// the number of messages encrypted with the same key to 2^32 (cf. [[1]])
	///
	/// [1]: https://nvlpubs.nist.gov/nistpubs/Legacy/SP/nistspecialpublication800-38d.pdf
	pub fn aes_128_gcm(key: &'a [u8; 16], nonce: &'a [u8; 12]) -> Builder<'a> {
		Builder {
			key: Key::Aes128Gcm(key),
			nonce,
			ad: &[],
			offset: 0
		}
	}

	/// AES-256 GCM mode encryption.
	///
	/// NOTE: The pair (key, nonce) must never be reused. Using random nonces limits
	/// the number of messages encrypted with the same key to 2^32 (cf. [[1]])
	///
	/// [1]: https://nvlpubs.nist.gov/nistpubs/Legacy/SP/nistspecialpublication800-38d.pdf
	pub fn aes_256_gcm(key: &'a [u8; 32], nonce: &'a [u8; 12]) -> Builder<'a> {
		Builder {
			key: Key::Aes256Gcm(key),
			nonce,
			ad: &[],
			offset: 0
		}
	}

	/// Optional associated data which is not encrypted but authenticated.
	pub fn associated_data(mut self, ad: &'a [u8]) -> Self {
		self.ad = ad;
		self
	}

	/// Optional offset value. Only the slice `[offset..]` will be used in
	/// `encrypt` or `decrypt`
	pub fn offset(mut self, off: usize) -> Self {
		self.offset = off;
		self
	}

	pub fn encrypt(self, mut data: Vec<u8>) -> Result<Vec<u8>, SymmError> {
		let (key, tag_len) = match self.key {
			Key::Aes128Gcm(key) => {
				let k = ring::aead::SealingKey::new(&ring::aead::AES_128_GCM, key)?;
				let n = ring::aead::AES_128_GCM.tag_len();
				(k, n)
			}
			Key::Aes256Gcm(key) => {
				let k = ring::aead::SealingKey::new(&ring::aead::AES_256_GCM, key)?;
				let n = ring::aead::AES_256_GCM.tag_len();
				(k, n)
			}
		};
		data.extend(::std::iter::repeat(0).take(tag_len));
		let len = ring::aead::seal_in_place(&key, self.nonce, self.ad, &mut data[self.offset ..], tag_len)?;
		data.truncate(self.offset + len);
		Ok(data)
	}

	pub fn decrypt(self, mut data: Vec<u8>) -> Result<Vec<u8>, SymmError> {
		let key = match self.key {
			Key::Aes128Gcm(key) => ring::aead::OpeningKey::new(&ring::aead::AES_128_GCM, key)?,
			Key::Aes256Gcm(key) => ring::aead::OpeningKey::new(&ring::aead::AES_256_GCM, key)?,
		};
		let len = ring::aead::open_in_place(&key, self.nonce, self.ad, 0, &mut data[self.offset ..])?.len();
		data.truncate(self.offset + len);
		Ok(data)
	}
}

#[cfg(test)]
mod tests {

	use super::Builder;

	#[test]
	fn aes_gcm_128() {
		let secret = b"1234567890123456";
		let nonce = b"123456789012";
		let message = b"So many books, so little time";

		let ciphertext = Builder::aes_128_gcm(secret, nonce)
			.encrypt(message.to_vec())
			.unwrap();

		assert!(ciphertext != message);

		let plaintext = Builder::aes_128_gcm(secret, nonce)
			.decrypt(ciphertext)
			.unwrap();

		assert_eq!(plaintext, message)
	}

	#[test]
	fn aes_gcm_256() {
		let secret = b"12345678901234567890123456789012";
		let nonce = b"123456789012";
		let message = b"So many books, so little time";

		let ciphertext = Builder::aes_256_gcm(secret, nonce)
			.encrypt(message.to_vec())
			.unwrap();

		assert!(ciphertext != message);

		let plaintext = Builder::aes_256_gcm(secret, nonce)
			.decrypt(ciphertext)
			.unwrap();

		assert_eq!(plaintext, message)
	}

	#[test]
	fn aes_gcm_256_offset() {
		let secret = b"12345678901234567890123456789012";
		let nonce = b"123456789012";
		let message = b"prefix data; So many books, so little time";

		let ciphertext = Builder::aes_256_gcm(secret, nonce)
			.offset(13) // length of "prefix data; "
			.encrypt(message.to_vec())
			.unwrap();

		assert!(ciphertext != &message[..]);

		let plaintext = Builder::aes_256_gcm(secret, nonce)
			.offset(13) // length of "prefix data; "
			.decrypt(ciphertext)
			.unwrap();

		assert_eq!(plaintext, &message[..])
	}
}

